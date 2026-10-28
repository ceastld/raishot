//! 主界面贴图页：显示器布局图、分组页签与当前分组的贴图列表。
//! 读写贴图工作区清单，锁定／穿透／定位走单张控制通道；本页不托管贴图窗口。
use super::{widgets, SettingsView};
use crate::{
    library::{self, PinState},
    pin_workspace::{
        self, Action, PinStatus, PinSummary as PinEntry, WorkspaceSummary as Workspace,
        DEFAULT_GROUP_ID,
    },
    pin_workspace_runtime::{self as runtime, InteractionAction, LiveControl, Outcome, Request},
    settings_widgets::ButtonKind,
    text_field::{TextField, TextFieldEvent},
    toolbar_icons::Icon,
    tray_request,
};
use gpui::{
    div, img, prelude::*, px, rgb, AnyElement, App, ClickEvent, Context, Entity, FocusHandle,
    FontWeight, KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit,
    Pixels, Point, Render, SharedString, Subscription, Window,
};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT},
    Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO},
    UI::WindowsAndMessaging::{
        GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER,
    },
};

const MAP_HEIGHT: f32 = 220.;
const MAP_PAD: f32 = 16.;
/// 管理器等待单张控制回执的上限，与通道期限一致。
const CONTROL_WAIT: Duration = Duration::from_secs(5);
/// 页面可见时重读清单与窗口位置的间隔。
const RELOAD_EVERY: Duration = Duration::from_millis(1000);
const CLOSED_STRIP: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}
impl Rect {
    fn right(self) -> i32 {
        self.x.saturating_add(self.w)
    }
    fn bottom(self) -> i32 {
        self.y.saturating_add(self.h)
    }
    fn area(self) -> i64 {
        i64::from(self.w.max(0)) * i64::from(self.h.max(0))
    }
    fn overlap(self, other: Rect) -> i64 {
        let w = self.right().min(other.right()) - self.x.max(other.x);
        let h = self.bottom().min(other.bottom()) - self.y.max(other.y);
        if w <= 0 || h <= 0 {
            0
        } else {
            i64::from(w) * i64::from(h)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Monitor {
    pub bounds: Rect,
    pub work: Rect,
    pub primary: bool,
}

/// 虚拟桌面上的全部显示器，物理像素（主界面进程按每显示器 DPI 感知）。
pub(crate) fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn visit(
        monitor: HMONITOR,
        _: HDC,
        _: *mut RECT,
        data: LPARAM,
    ) -> windows::core::BOOL {
        let list = unsafe { &mut *(data.0 as *mut Vec<Monitor>) };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            let rect = |r: RECT| Rect {
                x: r.left,
                y: r.top,
                w: r.right - r.left,
                h: r.bottom - r.top,
            };
            list.push(Monitor {
                bounds: rect(info.rcMonitor),
                work: rect(info.rcWork),
                primary: info.dwFlags & 1 != 0,
            });
        }
        windows::core::BOOL(1)
    }
    let mut list = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(visit),
            LPARAM(&mut list as *mut Vec<Monitor> as isize),
        );
    }
    // 主屏放最前，其余按位置排，编号稳定。
    list.sort_by_key(|m: &Monitor| (!m.primary, m.bounds.x, m.bounds.y));
    list
}

pub(crate) fn union(monitors: &[Monitor]) -> Option<Rect> {
    let first = monitors.first()?.bounds;
    let (mut left, mut top, mut right, mut bottom) =
        (first.x, first.y, first.right(), first.bottom());
    for monitor in &monitors[1..] {
        left = left.min(monitor.bounds.x);
        top = top.min(monitor.bounds.y);
        right = right.max(monitor.bounds.right());
        bottom = bottom.max(monitor.bounds.bottom());
    }
    Some(Rect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    })
}

/// 露在显示器内的面积不到五分之一，就算“在屏幕外”。
pub(crate) fn offscreen(rect: Rect, monitors: &[Monitor]) -> bool {
    let visible: i64 = monitors.iter().map(|m| rect.overlap(m.bounds)).sum();
    visible * 5 < rect.area()
}

/// 调回到工作区中央；比工作区大时贴左上角。
pub(crate) fn recall(rect: Rect, work: Rect) -> (i32, i32) {
    let x = work.x + ((work.w - rect.w) / 2).max(0);
    let y = work.y + ((work.h - rect.h) / 2).max(0);
    (x, y)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MapFit {
    pub scale: f32,
    pub ox: f32,
    pub oy: f32,
}
impl MapFit {
    pub(crate) fn x(self, value: i32) -> f32 {
        self.ox + value as f32 * self.scale
    }
    pub(crate) fn y(self, value: i32) -> f32 {
        self.oy + value as f32 * self.scale
    }
    pub(crate) fn len(self, value: i32) -> f32 {
        value as f32 * self.scale
    }
}
/// 把虚拟桌面等比放进 `width × height`，四周留 `pad`，居中。
pub(crate) fn fit(bounds: Rect, width: f32, height: f32, pad: f32) -> MapFit {
    let scale = ((width - pad * 2.) / bounds.w.max(1) as f32)
        .min((height - pad * 2.) / bounds.h.max(1) as f32)
        .max(0.001);
    MapFit {
        scale,
        ox: (width - bounds.w as f32 * scale) / 2. - bounds.x as f32 * scale,
        oy: (height - bounds.h as f32 * scale) / 2. - bounds.y as f32 * scale,
    }
}

struct PendingControl {
    request: Request,
    sent: Instant,
    title: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Rename {
    Pin(String),
    Group(String),
}
struct MapDrag {
    id: String,
    hwnd: isize,
    rect: Rect,
    start: Point<Pixels>,
    delta: (f32, f32),
    moved: bool,
}
#[derive(Clone)]
struct PinDrag {
    id: String,
    title: SharedString,
}
#[derive(Clone)]
struct GroupDrag {
    id: String,
    name: SharedString,
}
struct DragGhost(SharedString);
impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(rgb(palette.accent))
            .bg(rgb(surface.raised))
            .text_size(px(12.))
            .text_color(rgb(palette.foreground))
            .child(self.0.clone())
    }
}

/// 悬停提示，与拖动预览同一外观。
fn tip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> gpui::AnyView {
    let text = text.into();
    move |_, cx| cx.new(|_| DragGhost(text.clone())).into()
}

fn matches_query(title: &str, size: (i32, i32), query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || title.to_lowercase().contains(&query)
        || format!("{}x{}", size.0, size.1).contains(&query.replace('×', "x").replace(' ', ""))
}

pub(crate) struct PinsPage {
    workspace: Option<Workspace>,
    error: Option<String>,
    live: HashMap<String, LiveControl>,
    windows: HashMap<String, (isize, Rect)>,
    /// 窗口未打开、资源完整、可手动恢复的记录（不受启动恢复偏好限制）。
    restorable: HashSet<String>,
    monitors: Vec<Monitor>,
    loaded_at: Option<Instant>,
    loading: bool,
    load_serial: u64,
    pending_group_rename: Option<String>,
    selected: Option<String>,
    pending: HashMap<String, PendingControl>,
    rename: Option<Rename>,
    rename_field: Entity<TextField>,
    search: Entity<TextField>,
    drag: Option<MapDrag>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}
impl PinsPage {
    pub(super) fn new(window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        let rename_field = cx.new(|cx| TextField::new(String::new(), "", cx));
        rename_field.update(cx, |field, cx| {
            field.configure_focus(vec![rename_field.downgrade()], window, cx)
        });
        let subscription = cx.subscribe_in(
            &rename_field,
            window,
            |view: &mut SettingsView, _, event, window, cx| {
                if matches!(event, TextFieldEvent::Submit) {
                    view.pins_commit_rename(window, cx);
                }
            },
        );
        let search = cx.new(|cx| {
            TextField::new(
                String::new(),
                crate::tr!("搜索标题、尺寸", "Search title, size"),
                cx,
            )
        });
        search.update(cx, |field, cx| {
            field.configure_focus(vec![search.downgrade()], window, cx)
        });
        let search_subscription =
            cx.subscribe_in(&search, window, |_: &mut SettingsView, _, event, _, cx| {
                if matches!(event, TextFieldEvent::Changed) {
                    cx.notify();
                }
            });
        Self {
            workspace: None,
            loading: false,
            load_serial: 0,
            pending_group_rename: None,
            error: None,
            live: HashMap::new(),
            windows: HashMap::new(),
            restorable: HashSet::new(),
            monitors: Vec::new(),
            loaded_at: None,
            selected: None,
            pending: HashMap::new(),
            rename: None,
            rename_field,
            search,
            drag: None,
            focus: cx.focus_handle(),
            _subscriptions: vec![subscription, search_subscription],
        }
    }
    fn pin(&self, id: &str) -> Option<&PinEntry> {
        self.workspace
            .as_ref()?
            .pins
            .iter()
            .find(|pin| pin.id == id)
    }
    /// 窗口在屏幕上时用实际窗口矩形，否则用清单里最后保存的位置。
    fn rect(&self, pin: &PinEntry) -> Rect {
        self.windows
            .get(&pin.id)
            .map(|(_, rect)| *rect)
            .unwrap_or(Rect {
                x: pin.placement.x,
                y: pin.placement.y,
                w: pin.placement.width as i32,
                h: pin.placement.height as i32,
            })
    }
    fn state(&self, pin: &PinEntry) -> PinState {
        if pin.status == PinStatus::Closed {
            PinState::Closed
        } else if self.windows.contains_key(&pin.id) {
            PinState::Live
        } else {
            PinState::Idle
        }
    }
}

fn window_rect(hwnd: HWND) -> Option<Rect> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(Rect {
        x: rect.left,
        y: rect.top,
        w: rect.right - rect.left,
        h: rect.bottom - rect.top,
    })
}

fn outcome_text(action: InteractionAction, outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Success => match action {
            InteractionAction::Unlock => crate::tr!("已解除锁定", "Unlocked"),
            InteractionAction::Lock => crate::tr!("已锁定", "Locked"),
            InteractionAction::ClearClickThrough => {
                crate::tr!("已恢复鼠标交互", "Click-through turned off")
            }
            InteractionAction::EnableClickThrough => {
                crate::tr!("已开启鼠标穿透", "Click-through turned on")
            }
            InteractionAction::Locate => crate::tr!("已在屏幕上标出", "Shown on screen"),
        },
        Outcome::Stale => crate::tr!("窗口已变化，请重试", "The window changed; try again"),
        Outcome::Busy => crate::tr!(
            "贴图正在处理，请稍后再试",
            "The pin is busy; try again later"
        ),
        Outcome::Expired => crate::tr!("贴图没有及时响应", "The pin didn't respond in time"),
        Outcome::Failed => crate::tr!("贴图未能应用或保存", "The pin couldn't apply or save it"),
        Outcome::Closing => crate::tr!("贴图正在关闭", "The pin is closing"),
    }
}

impl SettingsView {
    /// 重读清单、窗口位置、实时控制状态与显示器；页面可见时每秒一次。
    pub(super) fn pins_reload(&mut self, cx: &mut Context<Self>) {
        self.pins.loaded_at = Some(Instant::now());
        self.pins.loading = true;
        self.pins.load_serial = self.pins.load_serial.wrapping_add(1);
        let serial = self.pins.load_serial;
        let task = cx
            .background_executor()
            .spawn(async { pin_workspace::load_summary().map_err(|error| error.to_string()) });
        cx.spawn(async move |this, cx| {
            let workspace = task.await;
            let _ = this.update(cx, |view, cx| view.pins_loaded(serial, workspace, cx));
        })
        .detach();
        cx.notify();
    }

    fn pins_loaded(
        &mut self,
        serial: u64,
        workspace: Result<Workspace, String>,
        cx: &mut Context<Self>,
    ) {
        if self.pins.load_serial != serial {
            return;
        }
        let active = self.page == super::MainPage::Pins;
        let page = &mut self.pins;
        page.loading = false;
        page.loaded_at = Some(Instant::now());
        page.monitors = monitors();
        match workspace {
            Ok(workspace) => {
                if !active {
                    page.workspace = Some(workspace);
                    page.error = None;
                    cx.notify();
                    return;
                }
                page.windows = runtime::windows_summary(&workspace)
                    .into_iter()
                    .filter_map(|(id, h)| window_rect(h).map(|rect| (id, (h.0 as isize, rect))))
                    .collect();
                page.live = runtime::active_controls_summary(&workspace);
                page.restorable = runtime::manual_restore_summary_ids(&workspace);
                page.error = None;
                page.pending
                    .retain(|id, _| workspace.pins.iter().any(|pin| pin.id == *id));
                if page
                    .selected
                    .as_deref()
                    .is_some_and(|id| !workspace.pins.iter().any(|pin| pin.id == id))
                {
                    page.selected = None;
                }
                let live = |id: &str| page.windows.contains_key(id);
                let jobs = library::pin_summary_items(&workspace, live)
                    .into_iter()
                    .map(|item| (item.key, item.source))
                    .collect();
                page.workspace = Some(workspace);
                self.ensure_thumbs(jobs, cx);
            }
            Err(error) => self.pins.error = Some(error.to_string()),
        }
        cx.notify();
    }

    /// 由主界面 250ms 轮询调用：收控制回执，按间隔重读。
    pub(super) fn pins_tick(&mut self, cx: &mut Context<Self>) {
        self.pins_poll(cx);
        let due = self
            .pins
            .loaded_at
            .is_none_or(|at| at.elapsed() >= RELOAD_EVERY);
        if due && !self.pins.loading && self.pins.drag.is_none() && self.pins.rename.is_none() {
            self.pins_reload(cx);
        }
    }

    fn pins_poll(&mut self, cx: &mut Context<Self>) {
        let mut finished = Vec::new();
        for (id, pending) in &self.pins.pending {
            let outcome = match runtime::control_result(&pending.request) {
                Ok(Some(outcome)) => Some(outcome),
                _ => (pending.sent.elapsed() >= CONTROL_WAIT).then_some(Outcome::Expired),
            };
            if let Some(outcome) = outcome {
                finished.push((
                    id.clone(),
                    pending.title.clone(),
                    pending.request.action,
                    outcome,
                ));
            }
        }
        if finished.is_empty() {
            return;
        }
        for (id, title, action, outcome) in finished {
            self.pins.pending.remove(&id);
            self.status = format!("{title}：{}", outcome_text(action, outcome));
        }
        self.pins_reload(cx);
    }

    fn pins_apply(&mut self, action: Action, message: &str, cx: &mut Context<Self>) -> bool {
        match pin_workspace::apply(action) {
            Ok(_) => {
                runtime::broadcast();
                self.status = message.into();
                self.pins_reload(cx);
                true
            }
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
                false
            }
        }
    }

    fn pins_request(&mut self, id: &str, action: InteractionAction, cx: &mut Context<Self>) {
        let title = self
            .pins
            .pin(id)
            .map(|pin| pin_workspace::display_pin_title(&pin.title))
            .unwrap_or_default();
        if self.pins.pending.contains_key(id) {
            self.status = crate::trf!(
                "{title}：上一个请求还在等待",
                "{title}: the previous request is still pending"
            );
            cx.notify();
            return;
        }
        // 点击时重新读取实时状态，旧渲染不能授权命令。
        let workspace = self.pins.workspace.clone();
        let live = workspace
            .as_ref()
            .and_then(|workspace| runtime::active_controls_summary(workspace).remove(id));
        let Some(live) = live else {
            self.status = crate::trf!(
                "{title}：窗口不在屏幕上或状态未知",
                "{title}: the window isn't on screen or its state is unknown"
            );
            cx.notify();
            return;
        };
        match runtime::request_control(id, &live, action) {
            Ok(request) => {
                self.pins.pending.insert(
                    id.to_owned(),
                    PendingControl {
                        request,
                        sent: Instant::now(),
                        title,
                    },
                );
            }
            Err(error) => self.status = format!("{title}：{error}"),
        }
        cx.notify();
    }

    fn pins_toggle_hidden(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(hidden) = self.pins.pin(id).map(|pin| !pin.hidden) else {
            return;
        };
        let message = if hidden {
            crate::tr!("已隐藏", "Hidden")
        } else {
            crate::tr!("已显示", "Shown")
        };
        self.pins_apply(
            Action::SetHidden {
                id: id.into(),
                hidden,
            },
            message,
            cx,
        );
    }

    fn pins_toggle_lock(&mut self, id: &str, cx: &mut Context<Self>) {
        let action = match self.pins.live.get(id) {
            Some(live) if live.locked => InteractionAction::Unlock,
            _ => InteractionAction::Lock,
        };
        self.pins_request(id, action, cx);
    }

    fn pins_toggle_through(&mut self, id: &str, cx: &mut Context<Self>) {
        let action = match self.pins.live.get(id) {
            Some(live) if live.click_through => InteractionAction::ClearClickThrough,
            _ => InteractionAction::EnableClickThrough,
        };
        self.pins_request(id, action, cx);
    }

    fn pins_close(&mut self, id: &str, cx: &mut Context<Self>) {
        self.status = match runtime::close_record(id) {
            Ok(()) => crate::tr!(
                "已关闭，可在下方“最近关闭”恢复",
                "Closed; restore it from Recently closed below"
            )
            .into(),
            Err(error) => crate::trf!("未能关闭：{error}", "Couldn't close: {error}"),
        };
        self.pins.loaded_at = None;
        cx.notify();
    }

    /// 关闭或未打开的贴图交给托盘启动贴图宿主恢复，含标注与撤销记录。
    fn pins_restore(&mut self, id: &str, cx: &mut Context<Self>) {
        self.status = match tray_request::send(tray_request::Request::PinRecord(id.into())) {
            Ok(()) => crate::tr!("正在恢复贴图", "Restoring the pin").into(),
            Err(error) => crate::trf!("未能恢复：{error}", "Couldn't restore: {error}"),
        };
        self.pins.loaded_at = None;
        cx.notify();
    }

    /// 逐张交给托盘恢复本组未打开的贴图；托盘一次最多排 16 个请求。
    fn pins_restore_idle(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let mut sent = 0;
        let mut failure = None;
        for id in ids.into_iter().take(16) {
            match tray_request::send(tray_request::Request::PinRecord(id)) {
                Ok(()) => sent += 1,
                Err(error) => failure = Some(error.to_string()),
            }
        }
        self.status = match failure {
            None => crate::trf!("正在恢复 {} 张贴图", "Restoring {} pins", sent),
            Some(error) => crate::trf!(
                "部分贴图未能恢复：{error}",
                "Some pins couldn't be restored: {error}"
            ),
        };
        self.pins.loaded_at = None;
        cx.notify();
    }

    fn pins_toggle_startup_restore(&mut self, cx: &mut Context<Self>) {
        let enabled = !self
            .pins
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.restore_on_startup);
        self.pins_apply(
            Action::SetRestore { enabled },
            if enabled {
                crate::tr!(
                    "启动时会恢复屏幕上的贴图",
                    "Pins on screen are restored at startup"
                )
            } else {
                crate::tr!(
                    "启动时不再恢复贴图",
                    "Pins are no longer restored at startup"
                )
            },
            cx,
        );
    }

    fn pins_move_window(&mut self, id: &str, x: i32, y: i32, cx: &mut Context<Self>) {
        let Some(&(hwnd, _)) = self.pins.windows.get(id) else {
            return;
        };
        // 贴图自己观察窗口位置变化并在下一次状态保存时记录。
        let moved = unsafe {
            SetWindowPos(
                HWND(hwnd as *mut _),
                None,
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        if let Err(error) = moved {
            self.status = crate::trf!("未能移动贴图：{error}", "Couldn't move the pin: {error}");
        }
        self.pins.loaded_at = None;
        cx.notify();
    }

    fn pins_recall(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(pin) = self.pins.pin(id) else {
            return;
        };
        let rect = self.pins.rect(pin);
        let Some(work) = self
            .pins
            .monitors
            .iter()
            .find(|m| m.primary)
            .or(self.pins.monitors.first())
            .map(|m| m.work)
        else {
            return;
        };
        let (x, y) = recall(rect, work);
        self.pins_move_window(id, x, y, cx);
        self.status = crate::tr!("已调回主屏中央", "Moved to the center of the main screen").into();
    }

    fn pins_switch_group(&mut self, id: &str, cx: &mut Context<Self>) {
        if self
            .pins
            .workspace
            .as_ref()
            .map(|w| w.current_group.as_str())
            == Some(id)
        {
            return;
        }
        self.pins.selected = None;
        self.pins_apply(
            Action::SwitchGroup { id: id.into() },
            crate::tr!("屏幕上已切换分组", "Switched the group on screen"),
            cx,
        );
    }

    fn pins_new_group(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let name = crate::tr!("新分组", "New group");
        match pin_workspace::apply(Action::Create { name: name.into() }) {
            Ok(workspace) => {
                self.pins.pending_group_rename = Some(workspace.current_group.clone());
                runtime::broadcast();
                self.status = crate::tr!("已新建分组", "Group created").into();
                self.pins_reload(cx);
            }
            Err(error) => {
                self.status = error.to_string();
                cx.notify();
            }
        }
    }

    fn pins_reorder(&mut self, dragged: &str, target: &str, cx: &mut Context<Self>) {
        let Some(workspace) = &self.pins.workspace else {
            return;
        };
        let mut ids: Vec<String> = workspace.groups.iter().map(|g| g.id.clone()).collect();
        let (Some(from), Some(to)) = (
            ids.iter().position(|id| id == dragged),
            ids.iter().position(|id| id == target),
        ) else {
            return;
        };
        if from == to {
            return;
        }
        let id = ids.remove(from);
        ids.insert(to, id);
        self.pins_apply(
            Action::Reorder { ids },
            crate::tr!("分组顺序已更新", "Group order updated"),
            cx,
        );
    }

    fn pins_start_rename(&mut self, target: Rename, window: &mut Window, cx: &mut Context<Self>) {
        let text = match (&target, &self.pins.workspace) {
            (Rename::Pin(id), _) => self
                .pins
                .pin(id)
                .map(|pin| match pin.title.as_str() {
                    "贴图" => String::new(),
                    title => title.to_owned(),
                })
                .unwrap_or_default(),
            (Rename::Group(id), Some(workspace)) => workspace
                .groups
                .iter()
                .find(|g| &g.id == id)
                .map(|g| g.display_name())
                .unwrap_or_default(),
            _ => String::new(),
        };
        self.pins
            .rename_field
            .update(cx, |field, cx| field.set_text(text, cx));
        window.focus(&self.pins.rename_field.read(cx).focus_handle);
        self.pins.rename = Some(target);
        cx.notify();
    }

    fn pins_commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.pins.rename.take() else {
            return;
        };
        let text = self.pins.rename_field.read(cx).text().trim().to_owned();
        window.focus(&self.pins.focus);
        match target {
            Rename::Pin(id) => {
                self.pins_apply(
                    Action::SetTitle { id, title: text },
                    crate::tr!("标题已更新", "Title updated"),
                    cx,
                );
            }
            Rename::Group(id) if !text.is_empty() => {
                self.pins_apply(
                    Action::Rename { id, name: text },
                    crate::tr!("分组名称已更新", "Group renamed"),
                    cx,
                );
            }
            Rename::Group(_) => cx.notify(),
        }
    }

    fn pins_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let search = self.pins.search.read(cx);
        let searching = search.focus_handle.is_focused(window);
        if search.is_composing() || self.pins.rename_field.read(cx).is_composing() {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if event.keystroke.key == "f"
            && modifiers.control
            && !modifiers.alt
            && !modifiers.shift
            && !modifiers.platform
        {
            window.focus(&self.pins.search.read(cx).focus_handle);
            cx.stop_propagation();
            return;
        }
        if modifiers.control || modifiers.alt || modifiers.shift || modifiers.platform {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                if self.pins.rename.take().is_some() {
                    window.focus(&self.pins.focus);
                } else if !self.pins.search.read(cx).text().is_empty() {
                    self.pins
                        .search
                        .update(cx, |field, cx| field.set_text(String::new(), cx));
                } else if searching {
                    window.focus(&self.pins.focus);
                } else {
                    self.pins.selected = None;
                }
                cx.notify();
                cx.stop_propagation();
            }
            "f2" if !searching && self.pins.rename.is_none() && !event.is_held => {
                if let Some(id) = self.pins.selected.clone() {
                    self.pins_start_rename(Rename::Pin(id), window, cx);
                    cx.stop_propagation();
                }
            }
            "f5" if !event.is_held => {
                self.pins_reload(cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    pub(super) fn pins_body(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if let Some(id) = self.pins.pending_group_rename.clone() {
            if self
                .pins
                .workspace
                .as_ref()
                .is_some_and(|workspace| workspace.groups.iter().any(|group| group.id == id))
            {
                self.pins.pending_group_rename = None;
                self.pins_start_rename(Rename::Group(id), window, cx);
            }
        }
        if self.pins.loaded_at.is_none() {
            self.pins_reload(cx);
        }
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let width = (f32::from(window.viewport_size().width) - super::sidebar_width(window) - 64.)
            .max(240.);
        let root = div()
            .id("pins-page")
            .size_full()
            .px_8()
            .pb_6()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_hidden()
            .track_focus(&self.pins.focus)
            .on_key_down(cx.listener(Self::pins_key));
        let Some(workspace) = self.pins.workspace.clone() else {
            let text = self
                .pins
                .error
                .clone()
                .unwrap_or_else(|| crate::tr!("正在读取…", "Loading…").into());
            return root
                .child(
                    div()
                        .pt_16()
                        .flex()
                        .justify_center()
                        .text_size(px(13.))
                        .text_color(rgb(palette.muted))
                        .child(text),
                )
                .into_any_element();
        };
        let current: Vec<&PinEntry> = workspace
            .pins
            .iter()
            .filter(|pin| {
                pin.status == PinStatus::Active && pin.group_id == workspace.current_group
            })
            .collect();
        let idle: Vec<String> = current
            .iter()
            .filter(|pin| {
                !self.pins.windows.contains_key(&pin.id) && self.pins.restorable.contains(&pin.id)
            })
            .map(|pin| pin.id.clone())
            .collect();
        root.child(self.pins_tabs(&workspace, idle, cx))
            .child(self.pins_map(&current, width, cx))
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .child(div().w(px(240.)).child(self.pins.search.clone())),
            )
            .child(self.pins_list(&current, cx))
            .child(self.pins_closed(&workspace, cx))
            .map(|root| {
                if self.pins.error.is_some() {
                    root.child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(palette.danger))
                            .child(self.pins.error.clone().unwrap_or_default()),
                    )
                } else {
                    root
                }
            })
            .bg(rgb(surface.page))
            .into_any_element()
    }

    fn pins_tabs(
        &self,
        workspace: &Workspace,
        idle: Vec<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut row = div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .border_b_1()
            .border_color(rgb(surface.line));
        for (index, group) in workspace.groups.iter().enumerate() {
            let id = group.id.clone();
            let current = group.id == workspace.current_group;
            let total = workspace
                .pins
                .iter()
                .filter(|p| p.group_id == group.id)
                .count();
            let active = workspace
                .pins
                .iter()
                .filter(|p| p.group_id == group.id && p.status == PinStatus::Active)
                .count();
            let renaming = self.pins.rename == Some(Rename::Group(group.id.clone()));
            let name: SharedString = group.display_name().into();
            let mut tab = div()
                .id(("pins-group", index))
                .h(px(36.))
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .cursor_pointer()
                .border_b_2()
                .border_color(rgb(if current {
                    palette.accent
                } else {
                    surface.page
                }))
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(if current {
                    palette.foreground
                } else {
                    palette.muted
                }))
                .hover(move |style| style.text_color(rgb(palette.foreground)))
                .child(
                    div()
                        .size(px(8.))
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(palette.accent))
                        .when(current, |dot| dot.bg(rgb(palette.accent))),
                );
            tab = if renaming {
                tab.child(div().w(px(140.)).child(self.pins.rename_field.clone()))
            } else {
                tab.child(name.clone())
            };
            tab = tab.child(
                div()
                    .text_size(px(12.))
                    .font_weight(FontWeight::NORMAL)
                    .text_color(rgb(palette.muted))
                    .child(active.to_string()),
            );
            if total == 0 && group.id != DEFAULT_GROUP_ID && !renaming {
                let delete_id = group.id.clone();
                tab = tab.child(
                    div()
                        .id(("pins-group-delete", index))
                        .size(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .hover(move |style| style.bg(rgb(palette.hover)))
                        .child(
                            crate::toolbar_icons::action(Icon::Close, palette.muted).size(px(12.)),
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            cx.stop_propagation();
                            view.pins_apply(
                                Action::DeleteEmptyGroup {
                                    id: delete_id.clone(),
                                },
                                crate::tr!("已删除空分组", "Empty group deleted"),
                                cx,
                            );
                        })),
                );
            }
            let drag = GroupDrag {
                id: group.id.clone(),
                name: name.clone(),
            };
            let drop_target = group.id.clone();
            let pin_target = group.id.clone();
            let rename_id = group.id.clone();
            row = row.child(
                tab.on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
                    if event.click_count() >= 2 {
                        view.pins_start_rename(Rename::Group(rename_id.clone()), window, cx);
                    } else {
                        view.pins_switch_group(&id, cx);
                    }
                }))
                .on_drag(drag, |drag: &GroupDrag, _, _, cx: &mut App| {
                    cx.new(|_| DragGhost(drag.name.clone()))
                })
                .drag_over::<PinDrag>(move |style, _, _, _| style.bg(rgb(palette.hover)))
                .drag_over::<GroupDrag>(move |style, _, _, _| style.bg(rgb(palette.hover)))
                .on_drop(cx.listener(move |view, drag: &GroupDrag, _, cx| {
                    view.pins_reorder(&drag.id, &drop_target, cx);
                }))
                .on_drop(cx.listener(move |view, drag: &PinDrag, _, cx| {
                    view.pins_apply(
                        Action::MovePin {
                            id: drag.id.clone(),
                            group_id: pin_target.clone(),
                        },
                        crate::tr!("已移到该分组", "Moved to the group"),
                        cx,
                    );
                })),
            );
        }
        row.child(
            div()
                .id("pins-group-new")
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(18.))
                .text_color(rgb(palette.muted))
                .hover(move |style| {
                    style
                        .bg(rgb(palette.hover))
                        .text_color(rgb(palette.foreground))
                })
                .child("+")
                .tooltip(tip(crate::tr!("新建分组", "New group")))
                .on_click(cx.listener(|view, _, window, cx| view.pins_new_group(window, cx))),
        )
        .child(div().flex_1())
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(palette.muted))
                .child(crate::tr!(
                    "单击切换屏幕上的分组 · 双击改名 · 拖动排序或把贴图拖进来",
                    "Click to switch · double-click to rename · drag to reorder or drop pins"
                )),
        )
        .when(!idle.is_empty(), |row| {
            let count = idle.len();
            row.child(
                widgets::button(
                    "pins-restore-idle",
                    crate::trf!("打开本组未打开的 {} 张", "Open {} not open", count),
                    ButtonKind::Secondary,
                    palette,
                    surface,
                )
                .ml_2()
                .on_click(
                    cx.listener(move |view, _, _, cx| view.pins_restore_idle(idle.clone(), cx)),
                ),
            )
        })
        .into_any_element()
    }

    fn pins_map(&self, current: &[&PinEntry], width: f32, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut map = div()
            .id("pins-map")
            .relative()
            .flex_shrink_0()
            .w_full()
            .h(px(MAP_HEIGHT))
            .rounded_lg()
            .border_1()
            .border_color(rgb(surface.line))
            .bg(rgb(surface.track))
            .overflow_hidden()
            .on_mouse_move(cx.listener(|view, event: &MouseMoveEvent, _, cx| {
                let Some(drag) = &mut view.pins.drag else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left) {
                    return;
                }
                let dx = f32::from(event.position.x - drag.start.x);
                let dy = f32::from(event.position.y - drag.start.y);
                if drag.moved || dx.abs() + dy.abs() > 3. {
                    drag.moved = true;
                    drag.delta = (dx, dy);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |view, _: &MouseUpEvent, _, cx| view.pins_drop_map(width, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |view, _: &MouseUpEvent, _, cx| view.pins_drop_map(width, cx)),
            );
        let Some(bounds) = union(&self.pins.monitors) else {
            return map.into_any_element();
        };
        let fit = fit(bounds, width, MAP_HEIGHT, MAP_PAD);
        for (index, monitor) in self.pins.monitors.iter().enumerate() {
            let b = monitor.bounds;
            map = map.child(
                div()
                    .absolute()
                    .left(px(fit.x(b.x)))
                    .top(px(fit.y(b.y)))
                    .w(px(fit.len(b.w)))
                    .h(px(fit.len(b.h)))
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(palette.border))
                    .bg(rgb(surface.page))
                    .child(
                        div()
                            .pl_2()
                            .pt_1()
                            .text_size(px(11.))
                            .text_color(rgb(palette.muted))
                            .child(if monitor.primary {
                                crate::trf!(
                                    "{} · 主屏 {}×{}",
                                    "{} · Main {}×{}",
                                    index + 1,
                                    b.w,
                                    b.h
                                )
                            } else {
                                format!("{} · {}×{}", index + 1, b.w, b.h)
                            }),
                    ),
            );
        }
        let mut offscreen_badges = 0;
        for pin in current {
            let Some(&(_, rect)) = self.pins.windows.get(&pin.id) else {
                continue;
            };
            let id = pin.id.clone();
            let title = pin_workspace::display_pin_title(&pin.title);
            if offscreen(rect, &self.pins.monitors) {
                let recall_id = pin.id.clone();
                let left = rect.x < bounds.x;
                map = map.child(
                    div()
                        .id(SharedString::from(format!("pins-offscreen-{}", pin.id)))
                        .absolute()
                        .top(px(8. + offscreen_badges as f32 * 30.))
                        .when(left, |badge| badge.left(px(8.)))
                        .when(!left, |badge| badge.right(px(8.)))
                        .h(px(26.))
                        .pl_2()
                        .pr_1()
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(0xe6b450))
                        .bg(rgb(surface.raised))
                        .text_size(px(11.))
                        .text_color(rgb(0xe6b450))
                        .child(crate::trf!("{title} 在屏幕外", "{title} is off screen"))
                        .child(
                            div()
                                .id(SharedString::from(format!("pins-recall-{}", pin.id)))
                                .px_2()
                                .rounded_full()
                                .cursor_pointer()
                                .bg(rgb(palette.hover))
                                .text_color(rgb(palette.foreground))
                                .child(crate::tr!("调回", "Bring back"))
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    view.pins_recall(&recall_id, cx)
                                })),
                        ),
                );
                offscreen_badges += 1;
                continue;
            }
            let selected = self.pins.selected.as_deref() == Some(pin.id.as_str());
            let (dx, dy) = match &self.pins.drag {
                Some(drag) if drag.id == pin.id && drag.moved => drag.delta,
                _ => (0., 0.),
            };
            let thumb = self.history_thumb(&format!("pin:{}", pin.id));
            let hwnd = self.pins.windows.get(&pin.id).map_or(0, |(h, _)| *h);
            map = map.child(
                div()
                    .id(SharedString::from(format!("pins-map-{}", pin.id)))
                    .absolute()
                    .left(px(fit.x(rect.x) + dx))
                    .top(px(fit.y(rect.y) + dy))
                    .w(px(fit.len(rect.w).max(6.)))
                    .h(px(fit.len(rect.h).max(6.)))
                    .rounded_sm()
                    .overflow_hidden()
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(if selected {
                        palette.accent
                    } else {
                        palette.muted
                    }))
                    .bg(rgb(surface.raised))
                    .when(pin.hidden, |rect| rect.opacity(0.35))
                    .when_some(thumb, |rect, image| {
                        rect.child(img(image).size_full().object_fit(ObjectFit::Cover))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                            window.focus(&view.pins.focus);
                            view.pins.selected = Some(id.clone());
                            view.pins.drag = Some(MapDrag {
                                id: id.clone(),
                                hwnd,
                                rect,
                                start: event.position,
                                delta: (0., 0.),
                                moved: false,
                            });
                            cx.notify();
                        }),
                    )
                    .tooltip(tip(title)),
            );
        }
        map.child(
            div()
                .absolute()
                .left(px(12.))
                .bottom(px(6.))
                .text_size(px(11.))
                .text_color(rgb(palette.muted))
                .child(crate::tr!(
                    "单击在屏幕上标出 · 拖动移动贴图 · 半透明为已隐藏",
                    "Click to show on screen · drag to move · faded = hidden"
                )),
        )
        .into_any_element()
    }

    /// 布局图松开鼠标：没拖动就标出贴图，拖动过就按比例移动真实窗口。
    fn pins_drop_map(&mut self, width: f32, cx: &mut Context<Self>) {
        let Some(drag) = self.pins.drag.take() else {
            return;
        };
        if !drag.moved {
            self.pins_request(&drag.id, InteractionAction::Locate, cx);
            return;
        }
        let Some(bounds) = union(&self.pins.monitors) else {
            return;
        };
        let scale = fit(bounds, width, MAP_HEIGHT, MAP_PAD).scale;
        let x = drag.rect.x + (drag.delta.0 / scale).round() as i32;
        let y = drag.rect.y + (drag.delta.1 / scale).round() as i32;
        if drag.hwnd != 0 {
            self.pins_move_window(&drag.id, x, y, cx);
            self.status = crate::tr!("已移动贴图", "Pin moved").into();
        }
    }

    fn pins_list(&self, current: &[&PinEntry], cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let mut list = div()
            .id("pins-list")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1();
        if current.is_empty() {
            return list
                .child(
                    div()
                        .pt_8()
                        .flex()
                        .justify_center()
                        .text_size(px(13.))
                        .text_color(rgb(palette.muted))
                        .child(crate::tr!(
                            "这个分组没有贴图。截图后按中键贴出，或把其它分组的贴图拖到页签上。",
                            "No pins in this group. Middle-click a capture to pin it, or drag pins onto a tab."
                        )),
                )
                .into_any_element();
        }
        let query = self.pins.search.read(cx).text();
        let mut sorted: Vec<&PinEntry> = current
            .iter()
            .copied()
            .filter(|pin| {
                let rect = self.pins.rect(pin);
                matches_query(
                    &pin_workspace::display_pin_title(&pin.title),
                    (rect.w, rect.h),
                    query,
                )
            })
            .collect();
        if sorted.is_empty() {
            return list
                .child(
                    div()
                        .pt_8()
                        .flex()
                        .justify_center()
                        .text_size(px(13.))
                        .text_color(rgb(palette.muted))
                        .child(crate::tr!("没有匹配的贴图", "No matching pins")),
                )
                .into_any_element();
        }
        sorted.sort_by_key(|pin| std::cmp::Reverse(library::pin_created(&pin.id)));
        for (index, pin) in sorted.into_iter().enumerate() {
            list = list.child(self.pins_row(index, pin, cx));
        }
        list.into_any_element()
    }

    fn pins_row(&self, index: usize, pin: &PinEntry, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let id = pin.id.clone();
        let selected = self.pins.selected.as_deref() == Some(pin.id.as_str());
        let state = self.pins.state(pin);
        let live = self.pins.live.get(&pin.id).copied();
        let pending = self.pins.pending.contains_key(&pin.id);
        let rect = self.pins.rect(pin);
        let off = state == PinState::Live && offscreen(rect, &self.pins.monitors);
        let title: SharedString = pin_workspace::display_pin_title(&pin.title).into();
        let renaming = self.pins.rename == Some(Rename::Pin(pin.id.clone()));
        let mut meta = div()
            .flex()
            .items_center()
            .gap_2()
            .text_size(px(12.))
            .text_color(rgb(palette.muted));
        let badge = |text: &'static str, color: u32| {
            div()
                .px(px(6.))
                .rounded_full()
                .border_1()
                .border_color(rgb(color))
                .text_color(rgb(color))
                .text_size(px(11.))
                .child(text)
        };
        if state == PinState::Idle {
            meta = meta.child(badge(crate::tr!("未打开", "Not open"), 0xe6b450));
        }
        if off {
            meta = meta.child(badge(crate::tr!("在屏幕外", "Off screen"), 0xe6b450));
        }
        if pin.hidden {
            meta = meta.child(badge(crate::tr!("已隐藏", "Hidden"), palette.muted));
        }
        if pin.text {
            meta = meta.child(badge(crate::tr!("文字", "Text"), palette.muted));
        }
        meta = meta.child(format!("{} × {}", rect.w, rect.h));
        if let Some(local) = library::pin_created(&pin.id).and_then(library::local_time) {
            meta = meta.child(library::date_text(&local));
        }
        let thumb = self.history_thumb(&format!("pin:{}", pin.id));
        let toggle =
            |key: &str, icon: Icon, on: bool, enabled: bool, warn: bool, hint: &'static str| {
                let color = if !enabled {
                    palette.border
                } else if on && warn {
                    0xe6b450
                } else if on {
                    palette.accent
                } else {
                    palette.muted
                };
                div()
                    .id(SharedString::from(format!("pins-{key}-{index}")))
                    .size(px(30.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .when(enabled, |button| {
                        button
                            .cursor_pointer()
                            .hover(move |style| style.bg(rgb(palette.hover)))
                    })
                    .child(crate::toolbar_icons::action(icon, color).size(px(18.)))
                    .tooltip(tip(hint))
            };
        let controllable = live.is_some_and(|live| !live.busy && !live.closing) && !pending;
        let locked = live.is_some_and(|live| live.locked) || pin.state.locked;
        let through = live.map_or(pin.click_through, |live| live.click_through);
        let mut tools = div().flex().items_center().gap_1();
        if state == PinState::Live {
            let (hide_id, lock_id, through_id, locate_id, close_id) =
                (id.clone(), id.clone(), id.clone(), id.clone(), id.clone());
            tools = tools
                .child(
                    toggle(
                        "hide",
                        if pin.hidden { Icon::EyeOff } else { Icon::Eye },
                        !pin.hidden,
                        true,
                        false,
                        if pin.hidden {
                            crate::tr!("显示", "Show")
                        } else {
                            crate::tr!("隐藏", "Hide")
                        },
                    )
                    .on_click(
                        cx.listener(move |view, _, _, cx| view.pins_toggle_hidden(&hide_id, cx)),
                    ),
                )
                .child(
                    toggle(
                        "lock",
                        if locked { Icon::Lock } else { Icon::Unlock },
                        locked,
                        controllable,
                        true,
                        if locked {
                            crate::tr!(
                                "已锁定：不能拖动、缩放和编辑，单击解锁",
                                "Locked: click to unlock"
                            )
                        } else {
                            crate::tr!("锁定", "Lock")
                        },
                    )
                    .when(controllable, |button| {
                        button.on_click(
                            cx.listener(move |view, _, _, cx| view.pins_toggle_lock(&lock_id, cx)),
                        )
                    }),
                )
                .child(
                    toggle(
                        "through",
                        Icon::ClickThrough,
                        through,
                        controllable,
                        true,
                        if through {
                            crate::tr!(
                                "鼠标穿透中，单击恢复交互",
                                "Click-through on: click to turn off"
                            )
                        } else {
                            crate::tr!("鼠标穿透", "Click-through")
                        },
                    )
                    .when(controllable, |button| {
                        button.on_click(cx.listener(move |view, _, _, cx| {
                            view.pins_toggle_through(&through_id, cx)
                        }))
                    }),
                )
                .child(
                    toggle(
                        "locate",
                        Icon::Target,
                        false,
                        controllable && !pin.hidden,
                        false,
                        crate::tr!("在屏幕上标出", "Show on screen"),
                    )
                    .when(controllable && !pin.hidden, |button| {
                        button.on_click(cx.listener(move |view, _, _, cx| {
                            view.pins_request(&locate_id, InteractionAction::Locate, cx)
                        }))
                    }),
                )
                .child(
                    toggle(
                        "close",
                        Icon::Close,
                        false,
                        true,
                        false,
                        crate::tr!("关闭（可恢复）", "Close (restorable)"),
                    )
                    .on_click(cx.listener(move |view, _, _, cx| view.pins_close(&close_id, cx))),
                );
        } else if self.pins.restorable.contains(&pin.id) {
            let restore_id = id.clone();
            tools = tools.child(
                widgets::button(
                    SharedString::from(format!("pins-open-{index}")),
                    crate::tr!("打开", "Open"),
                    ButtonKind::Secondary,
                    palette,
                    surface,
                )
                .on_click(cx.listener(move |view, _, _, cx| view.pins_restore(&restore_id, cx))),
            );
        } else {
            tools = tools.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(crate::tr!(
                        "图像缺失，无法打开",
                        "Image missing; can't open"
                    )),
            );
        }
        let rename_id = id.clone();
        let drag = PinDrag {
            id: id.clone(),
            title: title.clone(),
        };
        div()
            .id(SharedString::from(format!("pins-row-{index}")))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_3()
            .px_2()
            .py(px(6.))
            .rounded_lg()
            .when(selected, |row| row.bg(rgb(surface.nav_selected)))
            .when(!selected, |row| {
                row.hover(move |style| style.bg(rgb(surface.nav_hover)))
            })
            .child(
                div()
                    .w(px(72.))
                    .h(px(46.))
                    .flex_shrink_0()
                    .rounded_md()
                    .overflow_hidden()
                    .border_1()
                    .border_color(rgb(surface.line))
                    .bg(rgb(surface.track))
                    .when(pin.hidden || state != PinState::Live, |thumb| {
                        thumb.opacity(0.5)
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(|frame| match thumb {
                        Some(image) => {
                            frame.child(img(image).size_full().object_fit(ObjectFit::Cover))
                        }
                        None => frame.child(
                            crate::toolbar_icons::action(
                                if pin.text { Icon::Text } else { Icon::Pin },
                                palette.muted,
                            )
                            .size(px(18.)),
                        ),
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(if renaming {
                        div()
                            .w(px(280.))
                            .child(self.pins.rename_field.clone())
                            .into_any_element()
                    } else {
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(title.clone())
                            .into_any_element()
                    })
                    .child(meta),
            )
            .child(tools)
            .on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
                window.focus(&view.pins.focus);
                view.pins.selected = Some(rename_id.clone());
                if event.click_count() >= 2 {
                    view.pins_start_rename(Rename::Pin(rename_id.clone()), window, cx);
                }
                cx.notify();
            }))
            .on_drag(drag, |drag: &PinDrag, _, _, cx: &mut App| {
                cx.new(|_| DragGhost(drag.title.clone()))
            })
            .into_any_element()
    }

    fn pins_closed(&self, workspace: &Workspace, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let mut closed: Vec<&PinEntry> = workspace
            .pins
            .iter()
            .filter(|pin| {
                pin.status == PinStatus::Closed && pin.group_id == workspace.current_group
            })
            .collect();
        closed.sort_by_key(|pin| std::cmp::Reverse(pin.updated_ms));
        let mut strip = div()
            .id("pins-closed")
            .flex_1()
            .min_w(px(0.))
            .flex()
            .gap_2()
            .overflow_x_scroll();
        for (index, pin) in closed.iter().take(CLOSED_STRIP).enumerate() {
            let id = pin.id.clone();
            let thumb = self.history_thumb(&format!("pin:{}", pin.id));
            strip = strip.child(
                div()
                    .id(("pins-closed-item", index))
                    .w(px(64.))
                    .h(px(40.))
                    .flex_shrink_0()
                    .rounded_md()
                    .overflow_hidden()
                    .cursor_pointer()
                    .border_1()
                    .border_color(rgb(surface.line))
                    .bg(rgb(surface.track))
                    .opacity(0.8)
                    .hover(move |style| style.opacity(1.).border_color(rgb(palette.accent)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(|frame| match thumb {
                        Some(image) => {
                            frame.child(img(image).size_full().object_fit(ObjectFit::Cover))
                        }
                        None => frame.child(
                            crate::toolbar_icons::action(Icon::Pin, palette.muted).size(px(16.)),
                        ),
                    })
                    .tooltip(tip(crate::trf!(
                        "恢复“{}”",
                        "Restore \"{}\"",
                        pin_workspace::display_pin_title(&pin.title)
                    )))
                    .on_click(cx.listener(move |view, _, _, cx| view.pins_restore(&id, cx))),
            );
        }
        if closed.is_empty() {
            strip = strip.child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(crate::tr!("没有", "None")),
            );
        }
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_3()
            .pt_3()
            .border_t_1()
            .border_color(rgb(surface.line))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(crate::tr!("最近关闭", "Recently closed")),
            )
            .child(strip)
            .child(
                div()
                    .id("pins-closed-more")
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(palette.accent))
                    .hover(move |style| style.bg(rgb(palette.hover)))
                    .child(crate::tr!("在历史中查看全部", "See all in History"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.history_show_pins();
                        view.show_page(super::MainPage::History, cx);
                    })),
            )
            .child(div().w(px(1.)).h(px(20.)).bg(rgb(surface.line)))
            .child(
                div()
                    .id("pins-startup-restore")
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(palette.muted))
                    .child(widgets::switch(
                        workspace.restore_on_startup,
                        palette,
                        surface,
                    ))
                    .child(crate::tr!("启动时恢复", "Restore at startup"))
                    .tooltip(tip(crate::tr!(
                        "托盘启动时重新打开上次屏幕上的贴图",
                        "Reopen the pins that were on screen when the tray starts"
                    )))
                    .on_click(cx.listener(|view, _, _, cx| view.pins_toggle_startup_restore(cx))),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: i32, y: i32, w: i32, h: i32, primary: bool) -> Monitor {
        let bounds = Rect { x, y, w, h };
        Monitor {
            bounds,
            work: Rect {
                h: h - 40,
                ..bounds
            },
            primary,
        }
    }

    #[test]
    fn main_pins_offscreen_needs_a_fifth_visible_across_monitors() {
        let screens = [
            monitor(0, 0, 2560, 1440, true),
            monitor(2560, 200, 1920, 1080, false),
        ];
        let rect = |x, y| Rect {
            x,
            y,
            w: 1000,
            h: 500,
        };
        assert!(!offscreen(rect(100, 100), &screens));
        // 跨两屏仍算在屏幕上。
        assert!(!offscreen(rect(2000, 300), &screens));
        // 只露 10% 宽度。
        assert!(offscreen(rect(-900, 100), &screens));
        // 右侧屏上方的空隙不算显示区。
        assert!(offscreen(rect(3000, -450), &screens));
        assert_eq!(
            union(&screens),
            Some(Rect {
                x: 0,
                y: 0,
                w: 4480,
                h: 1440
            })
        );
    }

    #[test]
    fn main_pins_recall_centers_in_work_area_and_fit_keeps_aspect() {
        let work = Rect {
            x: -1920,
            y: 0,
            w: 1920,
            h: 1040,
        };
        assert_eq!(
            recall(
                Rect {
                    x: 5000,
                    y: 0,
                    w: 400,
                    h: 200
                },
                work
            ),
            (-1160, 420)
        );
        assert_eq!(
            recall(
                Rect {
                    x: 0,
                    y: 0,
                    w: 3000,
                    h: 2000
                },
                work
            ),
            (-1920, 0)
        );
        let bounds = Rect {
            x: -1920,
            y: 0,
            w: 3840,
            h: 1080,
        };
        let fit = fit(bounds, 800., 400., 16.);
        // 宽为瓶颈：768 / 3840 = 0.2；高度方向居中。
        assert!((fit.scale - 0.2).abs() < 1e-6);
        assert!((fit.x(-1920) - 16.).abs() < 1e-3);
        assert!((fit.x(1920) - 784.).abs() < 1e-3);
        assert!((fit.y(0) - (400. - 216.) / 2.).abs() < 1e-3);
        // 矮图框时高为瓶颈：(220 - 32) / 1080。
        let short = super::fit(bounds, 800., 220., 16.);
        assert!((short.scale - 188. / 1080.).abs() < 1e-6);
        assert!((short.y(0) - 16.).abs() < 1e-3);
    }
}
