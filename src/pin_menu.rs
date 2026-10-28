//! 贴图会话复用的不激活级联菜单；窗口区域仅覆盖主菜单和已展开的子菜单。
#[path = "pin_menu_hover.rs"]
mod hover;
#[path = "pin_menu_layout.rs"]
mod layout;
use crate::{
    gpui_capture::hwnd,
    ocr_markup::TextMark,
    pin_model::PinState,
    pin_native,
    pin_view::{PinAction, PinView},
    Result,
};
use gpui::{
    div, prelude::*, px, rgb, App, AppContext, Context, Render, ScrollHandle, Window, WindowBounds,
    WindowHandle, WindowOptions,
};
use windows::Win32::UI::Shell::{
    DefSubclassProc, GetWindowSubclass, RemoveWindowSubclass, SetWindowSubclass,
};
use windows::Win32::{
    Foundation::*,
    Graphics::Gdi::{
        CombineRgn, CreateRectRgn, CreateRoundRectRgn, DeleteObject, SetWindowRgn, RGN_ERROR,
        RGN_OR,
    },
    UI::{
        Input::KeyboardAndMouse::{GetCapture, ReleaseCapture},
        WindowsAndMessaging::*,
    },
};
const MENU_SUBCLASS: usize = 0x52534d4e;
const OWNER_SUBCLASS: usize = 0x52534d4f;
const ROW_INSET_LEFT: f32 = 4.;
const ROW_INSET_RIGHT: f32 = 8.;
const ROW_GAP: f32 = 6.;
const ROW_RADIUS: f32 = 4.;
const CHECK_WIDTH: f32 = 16.;
const SUBMENU_AIM_DELAY: std::time::Duration = std::time::Duration::from_millis(150);
type MenuItem = (&'static str, &'static str, PinAction, bool);
#[derive(Clone, Copy)]
enum MenuEntry {
    Action(usize, MenuItem),
    Submenu(u8),
    Separator,
}
#[derive(Clone, Copy, Default)]
struct Availability {
    mark: bool,
    selection: bool,
    paragraph: bool,
    table: bool,
}

/// 子菜单中每组首项的下标；组之间画分隔线。
fn group_starts(tab: u8) -> &'static [usize] {
    match tab {
        // 旋转翻转 | 色彩亮度 | 撤销重置
        1 => &[4, 8],
        // 窗口状态 | 尺寸缩放 | 滚轮中心 | 不透明度
        2 => &[4, 11, 14],
        // 文字选择 | 复制与识别 | 文字标记 | 标记颜色
        _ => &[2, 7, 12],
    }
}

fn next_selectable(enabled: &[bool], current: Option<usize>, forward: bool) -> Option<usize> {
    let len = enabled.len();
    for step in 1..=len {
        let index = match (current, forward) {
            (Some(index), true) => (index + step) % len,
            (Some(index), false) => (index + len - step) % len,
            (None, true) => step - 1,
            (None, false) => len - step,
        };
        if enabled[index] {
            return Some(index);
        }
    }
    None
}

unsafe extern "system" fn menu_native_proc(
    h: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    owner: usize,
) -> LRESULT {
    if message == WM_MOUSEACTIVATE {
        return LRESULT(MA_NOACTIVATE as isize);
    }
    if message == WM_NCDESTROY {
        let owner = HWND(owner as *mut _);
        let mut target = 0;
        if unsafe {
            GetWindowSubclass(
                owner,
                Some(owner_native_proc),
                OWNER_SUBCLASS,
                Some(&mut target),
            )
        }
        .as_bool()
            && target == h.0 as usize
        {
            unsafe {
                let _ = RemoveWindowSubclass(owner, Some(owner_native_proc), OWNER_SUBCLASS);
            }
        }
        unsafe {
            let _ = RemoveWindowSubclass(h, Some(menu_native_proc), id);
        }
    }
    unsafe { DefSubclassProc(h, message, wp, lp) }
}
unsafe extern "system" fn owner_native_proc(
    h: HWND,
    message: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    target: usize,
) -> LRESULT {
    if (message == WM_SHOWWINDOW && wp.0 == 0) || message == WM_NCDESTROY {
        let target = HWND(target as *mut _);
        let mut bound_owner = 0;
        // 只转发本 owner 隐藏事件给当前自有菜单，不访问 Rust/App 状态。
        if unsafe {
            IsWindow(Some(target)).as_bool()
                && GetWindowLongPtrW(target, GWLP_HWNDPARENT) == h.0 as isize
                && GetWindowThreadProcessId(target, None) == GetWindowThreadProcessId(h, None)
                && GetWindowSubclass(
                    target,
                    Some(menu_native_proc),
                    MENU_SUBCLASS,
                    Some(&mut bound_owner),
                )
                .as_bool()
                && bound_owner == h.0 as usize
        } {
            unsafe {
                let _ = PostMessageW(Some(target), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
    }
    if message == WM_NCDESTROY {
        unsafe {
            let _ = RemoveWindowSubclass(h, Some(owner_native_proc), id);
        }
    }
    unsafe { DefSubclassProc(h, message, wp, lp) }
}
fn install_native(menu: HWND, owner: HWND) -> Result<()> {
    unsafe {
        SetWindowSubclass(
            menu,
            Some(menu_native_proc),
            MENU_SUBCLASS,
            owner.0 as usize,
        )
        .ok()?;
        SetWindowSubclass(
            owner,
            Some(owner_native_proc),
            OWNER_SUBCLASS,
            menu.0 as usize,
        )
        .ok()?;
    }
    Ok(())
}
pub(crate) struct PinMenu {
    owner: WindowHandle<PinView>,
    state: PinState,
    annotation_preview: crate::pin_annotation::AnnotationPreviewVisibility,
    pub(crate) tab: u8,
    pub(crate) positions:
        std::rc::Rc<std::cell::RefCell<std::collections::HashMap<String, (f32, f32)>>>,
    through: bool,
    anchor: crate::pin_zoom::ZoomAnchor,
    pub(crate) requested_visible: bool,
    presentation_request: u64,
    pub(crate) scroll: ScrollHandle,
    root_scroll: ScrollHandle,
    layout: layout::Layout,
    focused: Option<(u8, usize)>,
    hover_request: u64,
    pending_tab: Option<u8>,
    last_pointer: Option<((f32, f32), std::time::Instant)>,
    region_request: u64,
    availability: Availability,
}
impl PinMenu {
    pub(crate) fn is_open(&self) -> bool {
        self.requested_visible
    }
    pub(crate) fn owner_matches(&self, owner: WindowHandle<PinView>) -> bool {
        self.owner == owner
    }
    pub(crate) fn dismiss(&mut self, w: &Window, cx: &mut Context<Self>) -> bool {
        let was_open = self.requested_visible;
        if !was_open {
            return false;
        }
        self.requested_visible = false;
        self.cancel_hover();
        self.last_pointer = None;
        self.region_request = self.region_request.wrapping_add(1);
        self.presentation_request = self.presentation_request.wrapping_add(1);
        // 菜单不激活宿主；隐藏后排队重绘宿主的选字活动端，避免重入借用。
        let owner = self.owner;
        cx.defer(move |cx| {
            let _ = owner.update(cx, |_, _, cx| cx.notify());
        });
        let generation = self.presentation_request;
        let menu = w.window_handle().downcast::<Self>().expect("贴图菜单类型");
        cx.spawn(async move |_, cx| {
            let target = menu
                .update(cx, |v, w, _| {
                    (!v.requested_visible && v.presentation_request == generation).then(|| hwnd(w))
                })
                .ok()
                .flatten();
            if let Some(target) = target {
                hide_native(target);
            }
        })
        .detach();
        true
    }
    fn probe(&self, id: String) -> impl IntoElement {
        let positions = self.positions.clone();
        gpui::canvas(
            move |b, _, _| {
                positions.borrow_mut().insert(
                    id.clone(),
                    (f32::from(b.center().x), f32::from(b.center().y)),
                );
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
    fn entries(&self, tab: u8) -> Vec<MenuEntry> {
        let items = self.items(tab);
        if tab == 0 {
            // 下标对应 items(0)：常用复制保存 | 三个子菜单 | 显示与工作区 | 隐藏关闭。
            // 段落复制、拾取颜色也在识别子菜单，主菜单不重复。
            let action = |index: usize| MenuEntry::Action(index, items[index]);
            return vec![
                action(1),
                action(3),
                action(4),
                action(5),
                action(6),
                action(7),
                MenuEntry::Separator,
                MenuEntry::Submenu(1),
                MenuEntry::Submenu(2),
                MenuEntry::Submenu(3),
                MenuEntry::Separator,
                action(9),
                action(10),
                action(11),
                action(14),
                action(0),
                MenuEntry::Separator,
                action(12),
                action(13),
            ];
        }
        let mut entries = Vec::new();
        for (index, item) in items.into_iter().enumerate() {
            if index > 0 && group_starts(tab).contains(&index) {
                entries.push(MenuEntry::Separator);
            }
            entries.push(MenuEntry::Action(index, item));
        }
        entries
    }
    fn submenu_panel(&self, tab: u8) -> layout::Panel {
        let entries = self.entries(tab);
        let separators = entries
            .iter()
            .filter(|entry| matches!(entry, MenuEntry::Separator))
            .count();
        self.layout.submenu(
            entries.len() - separators,
            separators,
            self.submenu_parent_y(tab),
        )
    }
    fn enabled(&self, entry: MenuEntry) -> bool {
        match entry {
            MenuEntry::Separator => false,
            MenuEntry::Submenu(_) => true,
            MenuEntry::Action(_, (_, _, action, _)) => match action {
                PinAction::MarkText(_) => self.availability.mark,
                PinAction::ClearTextSelection => self.availability.selection,
                PinAction::CopyParagraphText => self.availability.paragraph,
                PinAction::CopyTableText => self.availability.table,
                _ => true,
            },
        }
    }
    fn submenu_parent_y(&self, tab: u8) -> f32 {
        let entries = self.entries(0);
        let position = entries
            .iter()
            .position(|entry| matches!(entry, MenuEntry::Submenu(group) if *group == tab))
            .unwrap_or_default();
        let separators = entries[..position]
            .iter()
            .filter(|entry| matches!(entry, MenuEntry::Separator))
            .count();
        self.layout.root_row_y(position - separators, separators)
            + f32::from(self.root_scroll.offset().y)
    }
    fn regions(&self) -> Vec<crate::pin_model::Placement> {
        self.layout
            .regions((self.tab != 0).then(|| self.submenu_panel(self.tab)))
    }
    fn cancel_hover(&mut self) {
        self.hover_request = self.hover_request.wrapping_add(1);
        self.pending_tab = None;
    }
    fn hover_tab(
        &mut self,
        tab: u8,
        pointer: gpui::Point<gpui::Pixels>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if tab == self.tab {
            self.cancel_hover();
            return;
        }
        let aiming_at_submenu = self.tab != 0
            && self.last_pointer.is_some_and(|(previous, sampled)| {
                sampled.elapsed() <= SUBMENU_AIM_DELAY
                    && hover::moving_toward_submenu(
                        previous,
                        (f32::from(pointer.x), f32::from(pointer.y)),
                        self.submenu_panel(self.tab),
                        self.layout.submenu_left,
                    )
            });
        // 竖向浏览立即切换；只有斜向进入当前子菜单时保留短暂容错。
        // 每次移动都重新判断，转向其它项时不必等旧定时器到期。
        if !aiming_at_submenu {
            self.set_tab(tab, w, cx);
            return;
        }
        if self.pending_tab == Some(tab) {
            return;
        }
        self.cancel_hover();
        self.pending_tab = Some(tab);
        let request = self.hover_request;
        let menu = w.window_handle().downcast::<Self>().expect("贴图菜单类型");
        cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SUBMENU_AIM_DELAY).await;
            let _ = menu.update(cx, |v, w, cx| {
                if v.requested_visible && v.hover_request == request && v.pending_tab == Some(tab) {
                    v.set_tab(tab, w, cx);
                }
            });
        })
        .detach();
    }
    fn set_tab(&mut self, tab: u8, w: &Window, cx: &mut Context<Self>) {
        self.cancel_hover();
        if self.tab == tab {
            return;
        }
        if self
            .focused
            .is_some_and(|(focused_tab, _)| focused_tab != 0 && focused_tab != tab)
        {
            let group = if tab == 0 { self.tab } else { tab };
            self.focused = self
                .entries(0)
                .iter()
                .position(|entry| matches!(entry, MenuEntry::Submenu(t) if *t == group))
                .map(|index| (0, index));
        }
        self.tab = tab;
        self.scroll.set_offset(Default::default());
        self.positions.borrow_mut().clear();
        self.region_request = self.region_request.wrapping_add(1);
        let request = self.region_request;
        let generation = self.presentation_request;
        let previous_serial =
            unsafe { GetPropW(hwnd(w), windows::core::w!("RustShot.GpuiPresentSerial")) }.0
                as usize;
        let menu = w.window_handle().downcast::<Self>().expect("贴图菜单类型");
        cx.notify();
        cx.spawn(async move |_, cx| {
            let started = std::time::Instant::now();
            loop {
                let target = menu
                    .update(cx, |v, w, _| {
                        if !v.requested_visible
                            || v.presentation_request != generation
                            || v.region_request != request
                        {
                            return None;
                        }
                        let serial = unsafe {
                            GetPropW(hwnd(w), windows::core::w!("RustShot.GpuiPresentSerial"))
                        }
                        .0 as usize;
                        let ready = tab == 0 || serial != previous_serial;
                        Some((
                            hwnd(w),
                            ready.then(|| (v.regions(), v.layout.corner_diameter())),
                        ))
                    })
                    .ok()
                    .flatten();
                let Some((h, regions)) = target else {
                    return;
                };
                if let Some((regions, corner)) = regions {
                    if let Err(error) = apply_region(h, &regions, corner) {
                        eprintln!("贴图菜单区域：{error}");
                        let _ = menu.update(cx, |v, w, cx| v.dismiss(w, cx));
                    }
                    return;
                }
                if started.elapsed() >= std::time::Duration::from_secs(2) {
                    let _ = menu.update(cx, |v, w, cx| v.dismiss(w, cx));
                    return;
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(5))
                    .await;
            }
        })
        .detach();
    }
    fn activate(&mut self, entry: MenuEntry, w: &Window, cx: &mut Context<Self>) {
        if !self.enabled(entry) {
            return;
        }
        match entry {
            MenuEntry::Submenu(tab) => {
                self.set_tab(tab, w, cx);
                self.scroll.set_offset(Default::default());
                self.focused = next_selectable(
                    &self
                        .entries(tab)
                        .into_iter()
                        .map(|e| self.enabled(e))
                        .collect::<Vec<_>>(),
                    None,
                    true,
                )
                .map(|index| (tab, index));
                cx.notify();
            }
            MenuEntry::Action(_, (_, _, action, _)) => {
                let owner = self.owner;
                self.dismiss(w, cx);
                cx.defer(move |cx| {
                    let _ = owner.update(cx, |v, w, cx| v.perform(action, w, cx));
                });
            }
            MenuEntry::Separator => {}
        }
    }
    fn key(&mut self, e: &gpui::KeyDownEvent, w: &Window, cx: &mut Context<Self>) -> bool {
        if !self.requested_visible {
            return false;
        }
        let key = e.keystroke.key.as_str();
        let m = e.keystroke.modifiers;
        if m.control || m.alt || m.shift || m.platform || m.function {
            return false;
        }
        match key {
            "escape" => {
                self.dismiss(w, cx);
            }
            "up" | "down" | "home" | "end" => {
                self.cancel_hover();
                let tab = self.focused.map_or(0, |(tab, _)| tab);
                let entries = self.entries(tab);
                let enabled = entries.iter().map(|&e| self.enabled(e)).collect::<Vec<_>>();
                let current = if matches!(key, "home" | "end") {
                    None
                } else {
                    self.focused.map(|(_, index)| index)
                };
                let index = next_selectable(&enabled, current, matches!(key, "down" | "home"));
                self.focused = index.map(|index| (tab, index));
                if let Some(index) = index {
                    if tab == 0 {
                        let next_tab = match entries[index] {
                            MenuEntry::Submenu(tab) => tab,
                            _ => 0,
                        };
                        self.set_tab(next_tab, w, cx);
                        self.root_scroll.scroll_to_item(index);
                    } else {
                        self.scroll.scroll_to_item(index);
                    }
                }
            }
            "left" => {
                if self.tab != 0 {
                    let tab = self.tab;
                    self.set_tab(0, w, cx);
                    self.focused = self
                        .entries(0)
                        .iter()
                        .position(|entry| matches!(entry, MenuEntry::Submenu(t) if *t == tab))
                        .map(|index| (0, index));
                }
            }
            "right" | "enter" => {
                if let Some((tab, index)) = self.focused {
                    if let Some(&entry) = self.entries(tab).get(index) {
                        if key == "enter" || matches!(entry, MenuEntry::Submenu(_)) {
                            self.activate(entry, w, cx);
                        }
                    }
                }
            }
            _ => return false,
        }
        cx.notify();
        true
    }
    fn items(&self, tab: u8) -> Vec<MenuItem> {
        use PinAction::*;
        match tab {
            0 => vec![
                (
                    tr!("管理贴图…", "Manage pins…"),
                    "",
                    Workspace,
                    false,
                ),
                (
                    tr!("复制图像 / 所选文字", "Copy image / selected text"),
                    "Ctrl+C",
                    Copy,
                    false,
                ),
                (
                    tr!("段落复制", "Join lines"),
                    "Ctrl+Alt+C",
                    CopyParagraphText,
                    false,
                ),
                (tr!("图像另存为…", "Save image as…"), "Ctrl+S", Save, false),
                (
                    tr!("快速保存", "Quick save"),
                    "Ctrl+Shift+S",
                    QuickSave,
                    false,
                ),
                (
                    tr!("编辑 / 标注副本", "Edit / annotate a copy"),
                    "",
                    Edit,
                    false,
                ),
                (tr!("裁剪副本", "Crop a copy"), "", Crop, false),
                (
                    tr!("再贴一张", "Pin another copy"),
                    "Ctrl+D",
                    Duplicate,
                    false,
                ),
                (tr!("拾取图像颜色", "Pick image color"), "", Color, false),
                (
                    tr!("缩略收纳 / 展开", "Thumbnail / expand"),
                    tr!("Tab / 双击", "Tab / dbl-click"),
                    Thumbnail,
                    self.state.thumbnail,
                ),
                (tr!("显示操作栏", "Show toolbar"), "Space", Chrome, false),
                (
                    tr!("显示标注（仅预览）", "Show annotations (preview)"),
                    "H",
                    Annotations,
                    self.annotation_preview.visible(),
                ),
                (
                    tr!("隐藏贴图（从托盘恢复）", "Hide pin (restore from tray)"),
                    "",
                    Hide,
                    false,
                ),
                (
                    tr!("关闭贴图（可恢复）", "Close pin (restorable)"),
                    "Esc / Ctrl+W",
                    Close,
                    false,
                ),
                (tr!("设置标题…", "Set title…"), "F2", EditTitle, false),
            ],
            1 => vec![
                (
                    tr!("向右旋转 90°", "Rotate right 90°"),
                    "1",
                    Rotate(1),
                    false,
                ),
                (
                    tr!("向左旋转 90°", "Rotate left 90°"),
                    "2",
                    Rotate(-1),
                    false,
                ),
                (
                    tr!("水平翻转", "Flip horizontally"),
                    "3",
                    FlipX,
                    self.state.adjustments.flip_x,
                ),
                (
                    tr!("垂直翻转", "Flip vertically"),
                    "4",
                    FlipY,
                    self.state.adjustments.flip_y,
                ),
                (
                    tr!("灰度", "Grayscale"),
                    "5",
                    Gray,
                    self.state.adjustments.grayscale,
                ),
                (
                    tr!("反色", "Invert colors"),
                    "6",
                    Invert,
                    self.state.adjustments.invert,
                ),
                (
                    tr!("亮度 +10%", "Brightness +10%"),
                    "7",
                    Brightness(10),
                    false,
                ),
                (
                    tr!("亮度 −10%", "Brightness −10%"),
                    "8",
                    Brightness(-10),
                    false,
                ),
                (tr!("撤销处理", "Undo adjustment"), "Ctrl+Z", Undo, false),
                (tr!("重做处理", "Redo adjustment"), "Ctrl+Y", Redo, false),
                (
                    tr!("重置全部图像处理", "Reset all adjustments"),
                    "0",
                    ResetImage,
                    false,
                ),
            ],
            2 => vec![
                (
                    tr!("置顶", "Always on top"),
                    "T",
                    Topmost,
                    self.state.topmost,
                ),
                (
                    tr!("窗口阴影", "Window shadow"),
                    "Y",
                    Shadow,
                    self.state.shadow_enabled,
                ),
                (
                    tr!("锁定拖动、滚轮与编辑", "Lock drag, wheel and edits"),
                    "L",
                    Lock,
                    self.state.locked,
                ),
                (
                    tr!("鼠标穿透（托盘恢复交互）", "Click-through (undo from tray)"),
                    "Ctrl+T",
                    ClickThrough,
                    self.through,
                ),
                (tr!("适应当前屏幕", "Fit current screen"), "F", Fit, false),
                (
                    tr!("原始尺寸 100%", "Original size 100%"),
                    tr!("Ctrl+0 / 中键", "Ctrl+0 / middle-click"),
                    Original,
                    false,
                ),
                (tr!("缩放 25%", "Zoom 25%"), "", Zoom(0.25), false),
                (tr!("缩放 50%", "Zoom 50%"), "", Zoom(0.5), false),
                (tr!("缩放 150%", "Zoom 150%"), "", Zoom(1.5), false),
                (tr!("缩放 200%", "Zoom 200%"), "", Zoom(2.), false),
                (tr!("缩放 400%", "Zoom 400%"), "", Zoom(4.), false),
                (
                    tr!("滚轮中心：图片左上角", "Wheel anchor: image top-left"),
                    "",
                    WheelAnchor(crate::pin_zoom::ZoomAnchor::TopLeft),
                    self.anchor == crate::pin_zoom::ZoomAnchor::TopLeft,
                ),
                (
                    tr!("滚轮中心：鼠标位置", "Wheel anchor: pointer"),
                    "",
                    WheelAnchor(crate::pin_zoom::ZoomAnchor::Pointer),
                    self.anchor == crate::pin_zoom::ZoomAnchor::Pointer,
                ),
                (
                    tr!("滚轮中心：图片中心", "Wheel anchor: image center"),
                    "",
                    WheelAnchor(crate::pin_zoom::ZoomAnchor::Center),
                    self.anchor == crate::pin_zoom::ZoomAnchor::Center,
                ),
                (
                    tr!("不透明度 100%", "Opacity 100%"),
                    "",
                    Opacity(255),
                    self.state.opacity == 255,
                ),
                (
                    tr!("不透明度 75%", "Opacity 75%"),
                    "",
                    Opacity(191),
                    self.state.opacity == 191,
                ),
                (
                    tr!("不透明度 50%", "Opacity 50%"),
                    "",
                    Opacity(128),
                    self.state.opacity == 128,
                ),
                (
                    tr!("不透明度 25%", "Opacity 25%"),
                    "",
                    Opacity(64),
                    self.state.opacity == 64,
                ),
            ],
            _ => {
                let mut items = vec![
                    (
                        tr!("开启 / 关闭文字选择", "Toggle text selection"),
                        "E",
                        Text,
                        false,
                    ),
                    (
                        tr!("重新识别文字（本机）", "Recognize text again"),
                        "O",
                        Ocr,
                        false,
                    ),
                    (
                        tr!("复制全部文字", "Copy all text"),
                        "Ctrl+Shift+C",
                        CopyText,
                        false,
                    ),
                    (
                        tr!("段落复制", "Join lines"),
                        "Ctrl+Alt+C",
                        CopyParagraphText,
                        false,
                    ),
                    (tr!("复制表格", "Copy table"), "", CopyTableText, false),
                    (tr!("识别二维码", "Scan QR code"), "", Qr, false),
                    (tr!("拾取图像颜色", "Pick image color"), "", Color, false),
                ];
                items.extend(
                    [
                        TextMark::Highlight,
                        TextMark::Underline,
                        TextMark::Wave,
                        TextMark::Strike,
                    ]
                    .into_iter()
                    .map(|mark| (mark.label(), "", MarkText(mark), false)),
                );
                items.push((
                    tr!("清除文字选区", "Clear text selection"),
                    "",
                    ClearTextSelection,
                    false,
                ));
                items.extend([
                    (
                        tr!("下次标记：红色", "Next mark: red"),
                        "",
                        TextMarkColor(crate::toolbar_icons::TEXT_MARK_COLORS[1]),
                        false,
                    ),
                    (
                        tr!("下次标记：黄色", "Next mark: yellow"),
                        "",
                        TextMarkColor(crate::toolbar_icons::TEXT_MARK_COLORS[0]),
                        false,
                    ),
                    (
                        tr!("下次标记：绿色", "Next mark: green"),
                        "",
                        TextMarkColor(crate::toolbar_icons::TEXT_MARK_COLORS[3]),
                        false,
                    ),
                    (
                        tr!("下次标记：蓝色", "Next mark: blue"),
                        "",
                        TextMarkColor(crate::toolbar_icons::TEXT_MARK_COLORS[2]),
                        false,
                    ),
                    (
                        tr!("下次标记：黑色", "Next mark: black"),
                        "",
                        TextMarkColor(0x20252b),
                        false,
                    ),
                ]);
                items
            }
        }
    }
}
pub(crate) fn open(
    owner: WindowHandle<PinView>,
    state: PinState,
    annotation_preview: crate::pin_annotation::AnnotationPreviewVisibility,
    w: &mut Window,
    cx: &mut App,
) -> Result<WindowHandle<PinMenu>> {
    open_or_reuse(None, owner, state, annotation_preview, w, cx)
}
pub(crate) fn open_or_reuse(
    existing: Option<WindowHandle<PinMenu>>,
    owner: WindowHandle<PinView>,
    state: PinState,
    annotation_preview: crate::pin_annotation::AnnotationPreviewVisibility,
    w: &mut Window,
    cx: &mut App,
) -> Result<WindowHandle<PinMenu>> {
    let owner_hwnd = hwnd(w);
    let through = pin_native::transparent(owner_hwnd);
    let mut pointer = POINT::default();
    unsafe {
        GetCursorPos(&mut pointer)?;
    }
    let work = pin_native::work_area((pointer.x, pointer.y));
    let monitor = unsafe {
        windows::Win32::Graphics::Gdi::MonitorFromPoint(
            pointer,
            windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
        )
    };
    let mut dpi_x = 96;
    let mut dpi_y = 96;
    let dpi = if unsafe {
        windows::Win32::UI::HiDpi::GetDpiForMonitor(
            monitor,
            windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI,
            &mut dpi_x,
            &mut dpi_y,
        )
    }
    .is_ok()
        && dpi_x > 0
    {
        dpi_x as f32 / 96.
    } else {
        w.scale_factor()
    };
    let layout = layout::Layout::new((pointer.x, pointer.y), work, dpi);
    let placement = layout.placement;
    let existing = existing.filter(|menu| {
        menu.update(cx, |v, w, _| {
            v.owner_matches(owner) && unsafe { IsWindow(Some(hwnd(w))).as_bool() }
        })
        .unwrap_or(false)
    });
    let menu = if let Some(menu) = existing {
        menu
    } else {
        let (display_id, bounds) = pin_native::initial_pin_bounds(placement, cx);
        let initial_state = state.clone();
        let initial_preview = annotation_preview.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id,
                kind: gpui::WindowKind::PopUp,
                is_resizable: false,
                is_minimizable: false,
                is_movable: false,
                focus: false,
                show: false,
                titlebar: Some(gpui::TitlebarOptions {
                    appears_transparent: false,
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |w, cx| {
                w.set_window_title(brand_title!("贴图菜单", "Pin menu"));
                unsafe {
                    let h = hwnd(w);
                    SetWindowLongPtrW(h, GWL_STYLE, WS_POPUP.0 as isize);
                    SetWindowLongPtrW(
                        h,
                        GWL_EXSTYLE,
                        GetWindowLongPtrW(h, GWL_EXSTYLE)
                            | (WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0 as isize,
                    );
                    SetWindowLongPtrW(h, GWLP_HWNDPARENT, owner_hwnd.0 as isize);
                }
                w.disable_focus();
                let view = cx.new(|_| PinMenu {
                    owner,
                    state: initial_state,
                    annotation_preview: initial_preview,
                    tab: 0,
                    positions: Default::default(),
                    through,
                    anchor: crate::preferences::Preferences::load().pin_zoom_anchor,
                    requested_visible: false,
                    presentation_request: 0,
                    scroll: ScrollHandle::new(),
                    root_scroll: ScrollHandle::new(),
                    layout,
                    focused: None,
                    hover_request: 0,
                    pending_tab: None,
                    last_pointer: None,
                    region_request: 0,
                    availability: Default::default(),
                });
                let weak = view.downgrade();
                w.on_window_should_close(cx, move |w, cx| {
                    let _ = weak.update(cx, |v, cx| v.dismiss(w, cx));
                    false
                });
                view
            },
        )?
    };
    request_show(
        menu,
        state,
        annotation_preview,
        through,
        owner_hwnd,
        placement,
        layout,
        cx,
    )?;
    Ok(menu)
}
fn hide_native(h: HWND) {
    unsafe {
        // 此刻宿主可能已开始第一下拖动；只释放菜单自己仍持有的捕获。
        if GetCapture() == h {
            let _ = ReleaseCapture();
        }
        let _ = ShowWindow(h, SW_HIDE);
    }
}
fn apply_region(h: HWND, panels: &[crate::pin_model::Placement], corner: i32) -> Result<()> {
    unsafe {
        let region = CreateRectRgn(0, 0, 0, 0);
        if region.is_invalid() {
            return Err("无法创建贴图菜单区域".into());
        }
        for panel in panels {
            // 圆角区域的右下边界不含端点，因此各加 1 才能覆盖完整面板。
            let part = CreateRoundRectRgn(
                panel.x,
                panel.y,
                panel.x + panel.width as i32 + 1,
                panel.y + panel.height as i32 + 1,
                corner,
                corner,
            );
            if part.is_invalid() {
                let _ = DeleteObject(region.into());
                return Err("无法创建贴图子菜单区域".into());
            }
            let combined = CombineRgn(Some(region), Some(region), Some(part), RGN_OR);
            let _ = DeleteObject(part.into());
            if combined == RGN_ERROR {
                let _ = DeleteObject(region.into());
                return Err("无法合并贴图菜单区域".into());
            }
        }
        // 成功后 region 由 Windows 持有；失败时仍由调用者释放。
        if SetWindowRgn(h, Some(region), true) == 0 {
            let _ = DeleteObject(region.into());
            return Err("无法应用贴图菜单区域".into());
        }
    }
    Ok(())
}
fn request_show(
    menu: WindowHandle<PinMenu>,
    state: PinState,
    annotation_preview: crate::pin_annotation::AnnotationPreviewVisibility,
    through: bool,
    owner_hwnd: HWND,
    placement: crate::pin_model::Placement,
    layout: layout::Layout,
    cx: &mut App,
) -> Result<()> {
    let topmost = state.topmost;
    let anchor = crate::preferences::Preferences::load().pin_zoom_anchor;
    let generation = menu.update(cx, |v, _, cx| {
        v.state = state;
        v.annotation_preview = annotation_preview;
        v.through = through;
        v.anchor = anchor;
        v.tab = 0;
        v.layout = layout;
        v.focused = None;
        v.cancel_hover();
        v.last_pointer = None;
        v.region_request = v.region_request.wrapping_add(1);
        v.positions.borrow_mut().clear();
        v.scroll.set_offset(Default::default());
        v.root_scroll.set_offset(Default::default());
        v.requested_visible = true;
        v.presentation_request = v.presentation_request.wrapping_add(1);
        // 同步更新的可见状态由宿主下次绘制读取，通知必须在外层借用结束后执行。
        let owner = v.owner;
        cx.defer(move |cx| {
            let _ = owner.update(cx, |_, _, cx| cx.notify());
        });
        cx.notify();
        v.presentation_request
    })?;
    let owner_hwnd = owner_hwnd.0 as usize;
    cx.spawn(async move |cx| {
        let outcome: Result<()> = async {
            let target = menu.update(cx, |v, w, _| {
                (v.requested_visible && v.presentation_request == generation)
                    .then(|| (hwnd(w).0 as usize, v.owner))
            })?;
            let Some((target, owner)) = target else {
                return Ok(());
            };
            let h = HWND(target as *mut _);
            let owner_alive = owner
                .update(cx, |_, w, _| hwnd(w).0 as usize == owner_hwnd)
                .unwrap_or(false);
            if !owner_alive {
                return Err("贴图菜单显示前宿主已关闭".into());
            }
            install_native(h, HWND(owner_hwnd as *mut _))?;
            // 必须在 App/Window 借用之外定位，让真实 WM_SIZE 回填 GPUI viewport。
            unsafe {
                SetWindowPos(
                    h,
                    Some(if topmost {
                        HWND_TOPMOST
                    } else {
                        HWND_NOTOPMOST
                    }),
                    placement.x,
                    placement.y,
                    placement.width as i32,
                    placement.height as i32,
                    SWP_HIDEWINDOW | SWP_FRAMECHANGED | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                )?;
                // WM_DPICHANGED 可以应用推荐矩形；目标 DPI 已更新后只校正一次物理范围。
                if !pin_native::rect(h).is_ok_and(|actual| actual == placement) {
                    SetWindowPos(
                        h,
                        None,
                        placement.x,
                        placement.y,
                        placement.width as i32,
                        placement.height as i32,
                        SWP_HIDEWINDOW | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                    )?;
                }
                let mut client = RECT::default();
                GetClientRect(h, &mut client)?;
                let width = u16::try_from(client.right - client.left)?;
                let height = u16::try_from(client.bottom - client.top)?;
                PostMessageW(
                    Some(h),
                    WM_SIZE,
                    WPARAM(SIZE_RESTORED as usize),
                    LPARAM((u32::from(width) | (u32::from(height) << 16)) as isize),
                )?;
            }
            apply_region(h, &layout.regions(None), layout.corner_diameter())?;
            let previous_serial =
                unsafe { GetPropW(h, windows::core::w!("RustShot.GpuiPresentSerial")) }.0 as usize;
            menu.update(cx, |_, w, cx| {
                w.refresh();
                cx.notify();
            })?;
            let started = std::time::Instant::now();
            loop {
                let ready = menu.update(cx, |v, w, _| {
                    if !v.requested_visible || v.presentation_request != generation {
                        return None;
                    }
                    let current =
                        unsafe { GetPropW(h, windows::core::w!("RustShot.GpuiPresentSerial")) }.0
                            as usize;
                    let size = unsafe { GetPropW(h, windows::core::w!("RustShot.GpuiPresentSize")) }
                        .0 as usize as u32;
                    let viewport = w.viewport_size();
                    let mut client = RECT::default();
                    let client_matches = unsafe { GetClientRect(h, &mut client) }.is_ok()
                        && client.right - client.left == placement.width as i32
                        && client.bottom - client.top == placement.height as i32;
                    Some(
                        current != 0
                            && current != previous_serial
                            && size == ((placement.width << 16) | placement.height)
                            && (f32::from(viewport.width) * w.scale_factor()
                                - placement.width as f32)
                                .abs()
                                < 1.
                            && (f32::from(viewport.height) * w.scale_factor()
                                - placement.height as f32)
                                .abs()
                                < 1.
                            && client_matches
                            && pin_native::rect(h).is_ok_and(|actual| actual == placement),
                    )
                })?;
                match ready {
                    None => return Ok(()),
                    Some(true) => {
                        let owner_alive = owner
                            .update(cx, |v, w, _| {
                                hwnd(w).0 as usize == owner_hwnd
                                    && v.context_menu == Some(menu)
                                    && unsafe { IsWindowVisible(hwnd(w)).as_bool() }
                            })
                            .unwrap_or(false);
                        if !owner_alive {
                            return Err("贴图菜单首帧后宿主已关闭".into());
                        }
                        let still_requested = menu
                            .update(cx, |v, w, _| {
                                v.requested_visible
                                    && v.presentation_request == generation
                                    && hwnd(w) == h
                                    && v.owner_matches(owner)
                            })
                            .unwrap_or(false);
                        if !still_requested {
                            return Ok(());
                        }
                        unsafe {
                            SetWindowPos(
                                h,
                                None,
                                0,
                                0,
                                0,
                                0,
                                SWP_NOZORDER
                                    | SWP_NOMOVE
                                    | SWP_NOSIZE
                                    | SWP_NOACTIVATE
                                    | SWP_NOOWNERZORDER
                                    | SWP_SHOWWINDOW,
                            )?;
                        }
                        return Ok(());
                    }
                    Some(false) if started.elapsed() >= std::time::Duration::from_secs(2) => {
                        let snapshot = menu.update(cx, |v, w, _| {
                            let serial = unsafe { GetPropW(h, windows::core::w!("RustShot.GpuiPresentSerial")) }.0 as usize;
                            let packed_size = unsafe { GetPropW(h, windows::core::w!("RustShot.GpuiPresentSize")) }.0 as usize as u32;
                            let viewport = w.viewport_size();
                            let scale = w.scale_factor();
                            let mut client = RECT::default();
                            let client_valid = unsafe { GetClientRect(h, &mut client) }.is_ok();
                            let actual_rect = pin_native::rect(h).ok().map(|p| [
                                i64::from(p.x), i64::from(p.y), i64::from(p.width), i64::from(p.height),
                            ]);
                            serde_json::json!({
                                "phase": "pin_menu_first_frame_timeout",
                                "hwnd": h.0 as usize,
                                "generation": generation,
                                "current_generation": v.presentation_request,
                                "requested_visible": v.requested_visible,
                                "previous_serial": previous_serial,
                                "serial": serial,
                                "packed_present_size": packed_size,
                                "present_size": [packed_size >> 16, packed_size & 0xffff],
                                "scale_factor": scale,
                                "viewport_physical": [f32::from(viewport.width) * scale, f32::from(viewport.height) * scale],
                                "client_valid": client_valid,
                                "client_rect": [client.left, client.top, client.right, client.bottom],
                                "actual_rect": actual_rect,
                                "expected_rect": [i64::from(placement.x), i64::from(placement.y), i64::from(placement.width), i64::from(placement.height)],
                            })
                        }).ok();
                        if let Some(snapshot) = snapshot {
                            eprintln!("{snapshot}");
                        }
                        return Err("贴图菜单未获得正确尺寸的新首帧".into());
                    }
                    Some(false) => {}
                }
                // 隐藏 HWND 不保证产生 invalid region；沿用贴图首显的绘制唤醒。
                // 原请求/原窗口已在上方核验；Post 在 App/Window 借用之外，不显示或激活窗口。
                unsafe { PostMessageW(Some(h), WM_PAINT, WPARAM(0), LPARAM(0))?; }
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(5))
                    .await;
            }
        }
        .await;
        if let Err(error) = outcome {
            eprintln!("贴图菜单：{error}");
            let _ = menu.update(cx, |v, w, cx| {
                if v.presentation_request == generation {
                    v.dismiss(w, cx);
                }
            });
        }
    })
    .detach();
    Ok(())
}
impl PinMenu {
    fn panel(&mut self, tab: u8, rect: layout::Panel, cx: &mut Context<Self>) -> gpui::AnyElement {
        let palette = crate::theme::palette(cx);
        let mut items = div()
            .id(("pin-menu-panel", tab as usize))
            .absolute()
            .left(px(rect.x))
            .top(px(rect.y))
            .w(px(rect.width))
            .h(px(rect.height))
            .flex()
            .flex_col()
            .border_1()
            .border_color(rgb(palette.border))
            .rounded(px(layout::PANEL_RADIUS))
            .bg(rgb(palette.background))
            .overflow_y_scroll()
            .track_scroll(if tab == 0 {
                &self.root_scroll
            } else {
                &self.scroll
            })
            .p(px(layout::PANEL_PADDING - 1.))
            .on_mouse_move(cx.listener(move |v, _: &gpui::MouseMoveEvent, _, _| {
                if tab != 0 {
                    v.cancel_hover();
                }
            }))
            .on_scroll_wheel(cx.listener(move |v, _, w, cx| {
                if tab == 0 {
                    v.set_tab(0, w, cx);
                }
            }));
        let (can_mark, has_selection, can_copy_paragraph, can_copy_table) = self
            .owner
            .read(cx)
            .map_or((false, false, false, false), |owner| {
                (
                    owner.can_mark_selected_text(cx),
                    owner.has_selected_text(),
                    owner.can_copy_paragraph_text(cx),
                    owner.can_copy_table_text(cx),
                )
            });
        self.availability = Availability {
            mark: can_mark,
            selection: has_selection,
            paragraph: can_copy_paragraph,
            table: can_copy_table,
        };
        // 与宿主优先路由一致：展开且未锁定时 E/H 先走绘制工具，不能提示为宿主动作。
        let host_keys = self
            .owner
            .read(cx)
            .ok()
            .map(|owner| !owner.chrome || owner.state.locked);
        // 展开主栏时数字（含 Ctrl+0）只属于工具栏，图像处理数字键不生效，不提示；中键仍可用。
        let digit_keys = self.owner.read(cx).ok().map(|owner| !owner.chrome);
        for (index, entry) in self.entries(tab).into_iter().enumerate() {
            let (i, (label, key, action, checked)) = match entry {
                MenuEntry::Action(i, item) => (i, item),
                MenuEntry::Separator => {
                    items = items.child(
                        div()
                            .h(px(layout::SEPARATOR_HEIGHT))
                            .flex_none()
                            .flex()
                            .items_center()
                            .px(px(ROW_INSET_RIGHT))
                            .child(div().w_full().h(px(1.)).bg(rgb(palette.border))),
                    );
                    continue;
                }
                MenuEntry::Submenu(group) => {
                    let label = match group {
                        1 => tr!("图像", "Image"),
                        2 => tr!("显示", "Display"),
                        _ => tr!("识别", "Recognize"),
                    };
                    items = items.child(
                        div()
                            .id(("pin-menu-group", group as usize))
                            .relative()
                            .child(self.probe(format!("tab-{group}")))
                            .flex()
                            .items_center()
                            .gap(px(ROW_GAP))
                            .h(px(layout::ROW_HEIGHT))
                            .flex_none()
                            .pl(px(ROW_INSET_LEFT))
                            .pr(px(ROW_INSET_RIGHT))
                            .rounded(px(ROW_RADIUS))
                            .cursor_pointer()
                            .when(
                                self.tab == group || self.focused == Some((0, index)),
                                |row| row.bg(rgb(palette.selected)),
                            )
                            .hover(move |s| s.bg(rgb(palette.hover)))
                            .child(div().w(px(CHECK_WIDTH)).flex_none())
                            .child(div().flex_1().min_w(px(0.)).truncate().child(label))
                            .child(div().flex_none().text_color(rgb(palette.muted)).child(
                                if self.layout.submenu_left {
                                    "‹"
                                } else {
                                    "›"
                                },
                            ))
                            .on_hover(cx.listener(move |v, hovered: &bool, _, _| {
                                if !*hovered && v.pending_tab == Some(group) {
                                    v.cancel_hover();
                                }
                            }))
                            .on_mouse_move(cx.listener(
                                move |v, e: &gpui::MouseMoveEvent, w, cx| {
                                    let changed = v.focused != Some((0, index));
                                    v.focused = Some((0, index));
                                    v.hover_tab(group, e.position, w, cx);
                                    if changed {
                                        cx.notify();
                                    }
                                },
                            ))
                            .on_click(cx.listener(move |v, _, w, cx| {
                                v.activate(MenuEntry::Submenu(group), w, cx);
                            })),
                    );
                    continue;
                }
            };
            let digits_off = digit_keys != Some(true);
            // F2 同理：展开且未锁定时属于对象编辑，只从菜单进入标题编辑。
            let key = if (matches!(
                action,
                PinAction::Text | PinAction::Annotations | PinAction::EditTitle
            ) && host_keys != Some(true))
                || (crate::toolbar_groups::shortcut_digit(key).is_some() && digits_off)
            {
                ""
            } else if matches!(action, PinAction::Original) && digits_off {
                tr!("中键", "Middle-click")
            } else {
                key
            };
            let enabled = match action {
                PinAction::MarkText(_) => can_mark,
                PinAction::ClearTextSelection => has_selection,
                PinAction::CopyParagraphText => can_copy_paragraph,
                PinAction::CopyTableText => can_copy_table,
                _ => true,
            };
            let label = if matches!(action, PinAction::CopyTableText) && !enabled {
                tr!("复制表格（不可用）", "Copy table unavailable")
            } else {
                label
            };
            let probe = match action {
                PinAction::Zoom(1.5) => "pin-menu-zoom-150".into(),
                PinAction::WheelAnchor(crate::pin_zoom::ZoomAnchor::TopLeft) => {
                    "pin-menu-anchor-top-left".into()
                }
                PinAction::WheelAnchor(crate::pin_zoom::ZoomAnchor::Pointer) => {
                    "pin-menu-anchor-pointer".into()
                }
                PinAction::WheelAnchor(crate::pin_zoom::ZoomAnchor::Center) => {
                    "pin-menu-anchor-center".into()
                }
                PinAction::MarkText(mark) => match mark {
                    TextMark::Highlight => "pin-menu-mark-highlight".into(),
                    TextMark::Underline => "pin-menu-mark-underline".into(),
                    TextMark::Wave => "pin-menu-mark-wave".into(),
                    TextMark::Strike => "pin-menu-mark-strike".into(),
                },
                PinAction::ClearTextSelection => "pin-menu-clear-text-selection".into(),
                PinAction::CopyParagraphText => "pin-menu-copy-paragraph-text".into(),
                PinAction::CopyTableText => "pin-menu-copy-table-text".into(),
                PinAction::TextMarkColor(color) => format!("pin-menu-mark-color-{color:06x}"),
                _ => format!("item-{i}"),
            };
            items = items.child(
                div()
                    .id(("pin-menu-item", tab as usize * 100 + i))
                    .relative()
                    .child(self.probe(format!("item-{i}")))
                    .child(self.probe(probe))
                    .flex()
                    .items_center()
                    .gap(px(ROW_GAP))
                    .h(px(layout::ROW_HEIGHT))
                    .flex_none()
                    .pl(px(ROW_INSET_LEFT))
                    .pr(px(ROW_INSET_RIGHT))
                    .rounded(px(ROW_RADIUS))
                    .when(self.focused == Some((tab, index)), |row| row.bg(rgb(palette.selected)))
                    .opacity(if enabled { 1. } else { 0.45 })
                    .when(enabled, |item| {
                        item.cursor_pointer()
                            .hover(move |s| s.bg(rgb(palette.hover)))
                    })
                    .when(matches!(action, PinAction::Text | PinAction::Annotations), |item| {
                        item.tooltip(move |_, cx| {
                            let hint = match (action, host_keys) {
                                (PinAction::Text, Some(true)) => tr!("文字选择 · E", "Text selection · E"),
                                (PinAction::Text, _) => tr!("文字选择 · 收起绘制栏后按 E", "Text selection · close drawing bar for E"),
                                (_, Some(true)) => tr!("标注预览显隐 · H", "Show/hide annotations · H"),
                                _ => tr!("标注预览显隐 · 收起绘制栏后按 H", "Show/hide annotations · close drawing bar for H"),
                            };
                            cx.new(move |_| crate::pin_view::TextToolHint(hint)).into()
                        })
                    })
                    .when(matches!(action, PinAction::CopyTableText), |item| {
                        item.tooltip(move |_, cx| {
                            let hint = if enabled {
                                tr!("将完整识别结果复制为表格", "Copy the complete OCR result as a table")
                            } else {
                                tr!("没有识别出完整表格，或仍在输入；仍可复制原文", "No complete table was recognized, or input is in progress; you can still copy the text")
                            };
                            cx.new(move |_| crate::pin_view::TextToolHint(hint)).into()
                        })
                    })
                    .child(
                        div()
                            .w(px(CHECK_WIDTH))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .text_color(rgb(palette.accent))
                            .map(|mark| match action {
                                PinAction::TextMarkColor(color) => mark.child(
                                    div()
                                        .size(px(10.))
                                        .rounded_full()
                                        .border_1()
                                        .border_color(gpui::rgba(0x0000002e))
                                        .bg(rgb(color)),
                                ),
                                _ => mark.child(if checked { "✓" } else { "" }),
                            }),
                    )
                    .child(div().flex_1().min_w(px(0.)).truncate().child(label))
                    .child(
                        div()
                            .flex_none()
                            .when(!key.is_empty(), |key| key.ml(px(8.)))
                            .text_size(px(11.))
                            .text_color(rgb(palette.muted))
                            .child(key),
                    )
                    .on_hover(cx.listener(move |v, hovered: &bool, _, _| {
                        if tab == 0
                            && !*hovered
                            && v.pending_tab == Some(0)
                            && v.focused == Some((tab, index))
                        {
                            v.cancel_hover();
                        }
                    }))
                    .on_mouse_move(cx.listener(move |v, e: &gpui::MouseMoveEvent, w, cx| {
                        let changed = v.focused != Some((tab, index));
                        v.focused = Some((tab, index));
                        if tab == 0 { v.hover_tab(0, e.position, w, cx); } else { v.cancel_hover(); }
                        if changed { cx.notify(); }
                    }))
                    .on_click(cx.listener(move |v, _, w, cx| {
                        if !enabled {
                            return;
                        }
                        v.activate(entry, w, cx);
                    })),
            );
        }
        items.into_any_element()
    }
}
impl Render for PinMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let mut menu = div()
            .id("pin-menu")
            .relative()
            .size_full()
            .bg(rgb(palette.background))
            .text_color(rgb(palette.foreground))
            .font_family("Microsoft YaHei UI")
            .text_size(px(12.))
            // Bubble 顺序为行 → 根：行先判断方向，根最后记录本次坐标。
            .on_mouse_move(cx.listener(|v, e: &gpui::MouseMoveEvent, _, _| {
                v.last_pointer = Some((
                    (f32::from(e.position.x), f32::from(e.position.y)),
                    std::time::Instant::now(),
                ));
            }))
            .on_hover(cx.listener(|v, hovered: &bool, _, _| {
                if !*hovered {
                    v.cancel_hover();
                    v.last_pointer = None;
                }
            }))
            .child(self.panel(0, self.layout.root, cx))
            .child(
                div()
                    .absolute()
                    .left(px(self.layout.root.x + 3.))
                    .top(px(self.layout.root.y + self.layout.root.height / 2.))
                    .w(px(2.))
                    .h(px(2.))
                    .child(self.probe("pin-menu-background".into())),
            );
        if self.tab != 0 {
            let rect = self.submenu_panel(self.tab);
            menu = menu.child(self.panel(self.tab, rect, cx));
        }
        menu
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::next_selectable;

    #[test]
    fn keyboard_skips_separators_and_unavailable_commands_in_both_directions() {
        let rows = [true, false, false, true, false, true];
        assert_eq!(next_selectable(&rows, Some(0), true), Some(3));
        assert_eq!(next_selectable(&rows, Some(5), false), Some(3));
        assert_eq!(next_selectable(&rows, None, true), Some(0));
        assert_eq!(next_selectable(&rows, None, false), Some(5));
    }

    #[test]
    fn keyboard_wraps_at_menu_edges() {
        let rows = [false, true, false, true, false];
        assert_eq!(next_selectable(&rows, Some(3), true), Some(1));
        assert_eq!(next_selectable(&rows, Some(1), false), Some(3));
    }

    #[test]
    fn menu_without_available_commands_has_no_keyboard_target() {
        assert_eq!(next_selectable(&[], None, true), None);
        assert_eq!(next_selectable(&[false, false], Some(0), false), None);
    }
}

impl PinView {
    pub(crate) fn context_menu_key(&mut self, e: &gpui::KeyDownEvent, cx: &mut App) -> bool {
        self.context_menu
            .and_then(|menu| menu.update(cx, |v, w, cx| v.key(e, w, cx)).ok())
            .unwrap_or(false)
    }
    pub(crate) fn open_context_menu(
        &mut self,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let owner = w
            .window_handle()
            .downcast::<Self>()
            .ok_or("贴图宿主类型错误")?;
        let menu = match self.context_menu {
            Some(existing) => open_or_reuse(
                Some(existing),
                owner,
                self.state.clone(),
                self.annotation_preview.clone(),
                w,
                cx,
            )?,
            None => open(
                owner,
                self.state.clone(),
                self.annotation_preview.clone(),
                w,
                cx,
            )?,
        };
        self.context_menu = Some(menu);
        Ok(())
    }
    pub(crate) fn dismiss_context_menu(&mut self, cx: &mut App) -> bool {
        self.context_menu
            .and_then(|menu| menu.update(cx, |v, w, cx| v.dismiss(w, cx)).ok())
            .unwrap_or(false)
    }
    pub(crate) fn destroy_context_menu(&mut self, cx: &mut App) {
        self.context_menu_press = false;
        if let Some(menu) = self.context_menu.take() {
            cx.defer(move |cx| {
                let _ = menu.update(cx, |v, w, _| {
                    v.requested_visible = false;
                    v.presentation_request = v.presentation_request.wrapping_add(1);
                    w.remove_window();
                });
            });
        }
    }
    pub(crate) fn redirect_context_menu_wheel(
        &mut self,
        e: &gpui::ScrollWheelEvent,
        w: &Window,
        cx: &mut App,
    ) -> bool {
        let Some(menu) = self.context_menu else {
            return false;
        };
        let mut pointer = POINT {
            x: (f32::from(e.position.x) * w.scale_factor()).round() as i32,
            y: (f32::from(e.position.y) * w.scale_factor()).round() as i32,
        };
        if !unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(hwnd(w), &mut pointer) }
            .as_bool()
        {
            return false;
        }
        menu.update(cx, |v, w, cx| {
            let Ok(menu_rect) = pin_native::rect(hwnd(w)) else {
                return false;
            };
            let local = (pointer.x - menu_rect.x, pointer.y - menu_rect.y);
            if !v.requested_visible
                || !unsafe { IsWindowVisible(hwnd(w)).as_bool() }
                || !v.regions().iter().any(|p| {
                    local.0 >= p.x
                        && local.1 >= p.y
                        && i64::from(local.0) < i64::from(p.x) + i64::from(p.width)
                        && i64::from(local.1) < i64::from(p.y) + i64::from(p.height)
                })
            {
                return false;
            }
            // 关闭系统“悬停非活动窗口滚动”时，轮消息仍到有焦点的贴图；只滚菜单。
            let root = v.layout.root.physical(v.layout.scale);
            let in_root = local.0 >= root.x && local.0 < root.x + root.width as i32;
            if in_root {
                v.set_tab(0, w, cx);
            }
            let scroll = if in_root { &v.root_scroll } else { &v.scroll };
            let mut offset = scroll.offset();
            offset.y += e.delta.pixel_delta(px(16.)).y;
            offset.y = offset.y.clamp(-scroll.max_offset().height, px(0.));
            scroll.set_offset(offset);
            cx.notify();
            true
        })
        .unwrap_or(false)
    }
}
