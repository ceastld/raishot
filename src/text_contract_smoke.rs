//! 普通文字合同专项：自有图片编辑/Pin HWND，原生消息和生产输入适配器。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    capture_view::{Action, CaptureView},
    document::Object,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_model::PinState,
    pin_view::{PinAction, PinView},
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{App, AsyncApp, Context, EntityInputHandler, Window, WindowHandle};
use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
    time::Duration,
};
use windows::Win32::{
    Foundation::*,
    UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
};

const FOLDER: &str = "target/qa/text-contract";
const GROUPS: [(&str, &str); 5] = [
    ("enter", "native-text-enter-modifiers-unicode-gpu-export"),
    ("escape", "native-text-escape-edit-empty-undo"),
    ("held", "native-text-terminal-held-no-host-actions"),
    ("parameter", "native-text-parameter-return-keeps-draft"),
    ("ime", "native-text-ime-adapter-native-key-bypass"),
];
fn requested_groups() -> Result<Vec<&'static str>> {
    let value = std::env::var("RUSTSHOT_TEXT_CONTRACT_GROUPS").unwrap_or_default();
    if value.trim().is_empty() {
        return Ok(GROUPS.iter().map(|(_, full)| *full).collect());
    }
    let mut result = Vec::new();
    for token in value.split(',').map(str::trim) {
        let full = GROUPS
            .iter()
            .find(|(short, full)| *short == token || *full == token)
            .map(|(_, full)| *full)
            .ok_or_else(|| format!("未知普通文字专项组：{token}"))?;
        ensure(!result.contains(&full), "普通文字专项组重复")?;
        result.push(full);
    }
    Ok(result)
}
const TAG: isize = 1 << 28;
const CTRL: isize = 1 << 26;
const SHIFT: isize = 1 << 27;
const TAG_MASK: isize = TAG | CTRL | SHIFT;
const ADAPTER: usize = 0x52535443;
const FIRST_CHAR_UNITS: [u16; 5] = [0x4e2d, 0x6587, 0xd83d, 0xde42, 0x004d];
#[derive(Clone, serde::Serialize)]
struct CharArrival {
    hwnd: usize,
    thread_id: u32,
    qpc: i64,
    utf16_unit: u16,
}
#[derive(Default)]
pub(crate) struct AdapterStats {
    pub(crate) keys: Cell<u32>,
    pub(crate) restored: Cell<u32>,
    pub(crate) errors: Cell<u32>,
    char_probe_window: Cell<usize>,
    char_probe_next: Cell<usize>,
    char_probe_unmatched: Cell<u32>,
    char_arrivals: RefCell<Vec<CharArrival>>,
}

// 仅显式文字专项的第一串私有标记+5单位序列：消息回调只写有界内存，不输出日志。
struct CharProbe(Rc<AdapterStats>);
impl CharProbe {
    fn begin(input: Input, stats: &Rc<AdapterStats>) -> Self {
        stats.char_arrivals.borrow_mut().clear();
        stats.char_probe_next.set(0);
        stats.char_probe_unmatched.set(0);
        stats.char_probe_window.set(input.handle);
        Self(stats.clone())
    }
    fn finish(self) -> (Vec<CharArrival>, u32) {
        self.0.char_probe_window.set(0);
        let arrivals = std::mem::take(&mut *self.0.char_arrivals.borrow_mut());
        (arrivals, self.0.char_probe_unmatched.get())
    }
}
impl Drop for CharProbe {
    fn drop(&mut self) {
        self.0.char_probe_window.set(0);
    }
}

// SetKeyboardState 仅调整当前 UI 线程的键盘状态。每个 tagged 自有 HWND 消息保存/恢复。
unsafe extern "system" fn tagged_key(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    if msg == WM_NCDESTROY {
        let result = DefSubclassProc(h, msg, wp, lp);
        let _ = RemoveWindowSubclass(h, Some(tagged_key), id);
        drop(Box::from_raw(data as *mut Rc<AdapterStats>));
        return result;
    }
    if msg == WM_CHAR {
        let stats = (*(data as *const Rc<AdapterStats>)).clone();
        if stats.char_probe_window.get() == h.0 as usize {
            let next = stats.char_probe_next.get();
            if lp.0 == TAG && FIRST_CHAR_UNITS.get(next).copied().map(usize::from) == Some(wp.0) {
                stats.char_probe_next.set(next + 1);
                stats.char_arrivals.borrow_mut().push(CharArrival {
                    hwnd: h.0 as usize,
                    thread_id: windows::Win32::System::Threading::GetCurrentThreadId(),
                    qpc: crate::capture_standby::qpc(),
                    utf16_unit: wp.0 as u16,
                });
            } else {
                // 未匹配消息仅记数量，不能把其他用户字符写入诊断。
                stats
                    .char_probe_unmatched
                    .set(stats.char_probe_unmatched.get().saturating_add(1));
            }
        }
        return DefSubclassProc(h, msg, wp, if lp.0 == TAG { LPARAM(0) } else { lp });
    }
    if msg != WM_KEYDOWN || lp.0 & TAG == 0 {
        return DefSubclassProc(h, msg, wp, lp);
    }
    // 本次 DefSubclassProc 可能同步销毁参数 HWND；保留 Rc，不能借用已释放的 refdata。
    let stats = (*(data as *const Rc<AdapterStats>)).clone();
    let mut saved = [0u8; 256];
    if GetKeyboardState(&mut saved).is_err() {
        stats.errors.set(stats.errors.get() + 1);
        return LRESULT(0);
    }
    let mut local = saved;
    for k in [
        VK_CONTROL,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_SHIFT,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_MENU,
        VK_LMENU,
        VK_RMENU,
        VK_LWIN,
        VK_RWIN,
    ] {
        local[k.0 as usize] &= 0x7f;
    }
    if lp.0 & CTRL != 0 {
        local[VK_CONTROL.0 as usize] |= 0x80;
        local[VK_LCONTROL.0 as usize] |= 0x80;
    }
    if lp.0 & SHIFT != 0 {
        local[VK_SHIFT.0 as usize] |= 0x80;
        local[VK_LSHIFT.0 as usize] |= 0x80;
    }
    if SetKeyboardState(&local).is_err() {
        stats.errors.set(stats.errors.get() + 1);
        let _ = SetKeyboardState(&saved);
        return LRESULT(0);
    }
    stats.keys.set(stats.keys.get() + 1);
    let result = DefSubclassProc(h, msg, wp, LPARAM(lp.0 & !TAG_MASK));
    let mut restored = [0u8; 256];
    if SetKeyboardState(&saved).is_ok()
        && GetKeyboardState(&mut restored).is_ok()
        && restored == saved
    {
        stats.restored.set(stats.restored.get() + 1);
    } else {
        stats.errors.set(stats.errors.get() + 1);
    }
    result
}
pub(crate) fn install(input: Input, stats: &Rc<AdapterStats>) -> Result<()> {
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    unsafe {
        let h = HWND(input.handle as *mut _);
        let mut existing = 0usize;
        if GetWindowSubclass(h, Some(tagged_key), ADAPTER, Some(&mut existing)).as_bool() {
            return Ok(());
        }
        let data = Box::into_raw(Box::new(stats.clone()));
        if !SetWindowSubclass(h, Some(tagged_key), ADAPTER, data as usize).as_bool() {
            drop(Box::from_raw(data));
            return Err("自有文字消息 adapter 安装失败".into());
        }
    }
    Ok(())
}
pub(crate) fn key(
    input: Input,
    vk: VIRTUAL_KEY,
    control: bool,
    shift: bool,
    held: bool,
) -> Result<()> {
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            WM_KEYDOWN,
            WPARAM(vk.0 as usize),
            LPARAM(
                TAG | if control { CTRL } else { 0 }
                    | if shift { SHIFT } else { 0 }
                    | if held { 1 << 30 } else { 0 },
            ),
        )?;
    }
    Ok(())
}
fn chars(input: Input, text: &str) -> Result<()> {
    for unit in text.encode_utf16() {
        unsafe {
            PostMessageW(
                Some(HWND(input.handle as *mut _)),
                WM_CHAR,
                WPARAM(unit as usize),
                LPARAM(0),
            )?;
        }
    }
    Ok(())
}
fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(60))
        .await;
}
async fn settle(cx: &mut AsyncApp) {
    tick(cx).await;
    tick(cx).await;
}
fn fixture() -> Image {
    Image {
        width: 720,
        height: 420,
        bgra: vec![255; 720 * 420 * 4],
        readback_ms: 0.,
    }
}

#[derive(Clone, Copy)]
enum Host {
    Image(WindowHandle<CaptureView>),
    Pin(WindowHandle<PinView>),
}
impl Host {
    fn name(self) -> &'static str {
        match self {
            Self::Image(_) => "image_editor",
            Self::Pin(_) => "pin",
        }
    }
    fn update<T>(
        self,
        cx: &mut AsyncApp,
        f: impl FnOnce(&mut CaptureView, &mut Window, &mut Context<CaptureView>) -> T,
    ) -> Result<T> {
        match self {
            Self::Image(h) => h.update(cx, f).map_err(|e| e.to_string().into()),
            Self::Pin(h) => h
                .update(cx, |v, w, cx| {
                    v.canvas
                        .as_ref()
                        .expect("专项自有 Pin canvas")
                        .update(cx, |c, cx| f(c, w, cx))
                })
                .map_err(|e| e.to_string().into()),
        }
    }
    fn input(self, cx: &mut AsyncApp) -> Result<Input> {
        self.update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
    }
    fn action(self, a: Action, cx: &mut AsyncApp) -> Result<()> {
        match self {
            Self::Image(_) => self.update(cx, |c, w, cx| c.perform(a, w, cx)),
            Self::Pin(h) => h
                .update(cx, |v, w, cx| v.annotation(a, w, cx))
                .map_err(|e| e.to_string().into()),
        }
    }
    fn chrome(self, cx: &mut AsyncApp) -> Result<(bool, Option<usize>, bool, bool)> {
        match self {
            Self::Image(_) => self.update(cx, |c, _, _| (true, None, c.saving, c.processing)),
            Self::Pin(h) => h
                .update(cx, |v, _, cx| {
                    let bar = v
                        .toolbar
                        .and_then(|h| h.update(cx, |_, w, _| hwnd(w).0 as usize).ok());
                    (
                        v.chrome,
                        bar,
                        v.saving || v.canvas.as_ref().is_some_and(|c| c.read(cx).saving),
                        v.canvas.as_ref().is_some_and(|c| c.read(cx).processing),
                    )
                })
                .map_err(|e| e.to_string().into()),
        }
    }
    fn property_input(self, cx: &mut AsyncApp) -> Result<Input> {
        match self {
            Self::Image(_) => self.input(cx),
            Self::Pin(h) => h
                .update(cx, |v, _, cx| -> Result<Input> {
                    let bar = v.toolbar.ok_or("文字一级栏缺失")?;
                    bar.update(cx, |b, _, cx| -> Result<Input> {
                        if let Some(p) = b.popup(cx) {
                            Ok(p.update(cx, |_, w, _| Input {
                                handle: hwnd(w).0 as usize,
                                scale: w.scale_factor(),
                            })?)
                        } else {
                            Ok(b.properties
                                .ok_or("文字二级栏缺失")?
                                .update(cx, |_, w, _| Input {
                                    handle: hwnd(w).0 as usize,
                                    scale: w.scale_factor(),
                                })?)
                        }
                    })
                    .map_err(|e| e.to_string())?
                })
                .map_err(|e| e.to_string())?,
        }
    }
    fn point(self, p: Point, cx: &mut AsyncApp) -> Result<(u32, u32)> {
        self.update(cx, |c, w, _| {
            let p = c.canvas_viewport(w).window_point(p);
            (p.x.round() as u32, p.y.round() as u32)
        })
    }
    fn output(self, cx: &mut AsyncApp) -> Result<Image> {
        match self {
            Self::Image(_) => self.update(cx, |c, _, _| {
                c.try_finish_content()?;
                c.selected_image()
            })?,
            Self::Pin(h) => h
                .update(cx, |v, _, cx| {
                    v.user_export_snapshot(cx).and_then(|s| s.render())
                })
                .map_err(|e| e.to_string())?,
        }
    }
    fn remove(self, cx: &mut AsyncApp) {
        match self {
            Self::Image(h) => {
                let _ = h.update(cx, |_, w, _| w.remove_window());
            }
            Self::Pin(h) => {
                let _ = h.update(cx, |v, w, cx| {
                    v.close_toolbar(cx);
                    w.remove_window();
                });
            }
        }
    }
}
#[derive(Debug, PartialEq)]
struct State {
    draft: Option<(Option<u64>, Annotation)>,
    items: Vec<Object>,
    revision: u64,
    edit_revision: u64,
    tool: Tool,
    selection: std::ops::Range<usize>,
    marked: Option<std::ops::Range<usize>>,
    undo: bool,
    redo: bool,
}
fn state(host: Host, cx: &mut AsyncApp) -> Result<State> {
    host.update(cx, |c, _, _| State {
        draft: c
            .editor
            .text_draft
            .as_ref()
            .map(|d| (d.id, d.annotation.clone())),
        items: c.editor.document.items().to_vec(),
        revision: c.editor.document.revision(),
        edit_revision: c.editor.document.edit_revision(),
        tool: c.editor.tool,
        selection: c.input.selection.clone(),
        marked: c.input.marked.clone(),
        undo: c.editor.document.can_undo(),
        redo: c.editor.document.can_redo(),
    })
}
fn draft_text(host: Host, cx: &mut AsyncApp) -> Result<String> {
    host.update(cx, |c, _, _| {
        c.editor
            .text_draft
            .as_ref()
            .map(|d| d.annotation.text.clone())
            .ok_or_else(|| "文字草稿意外结束".into())
    })?
}
fn first_char_snapshot(host: Host, cx: &mut AsyncApp) -> Result<serde_json::Value> {
    host.update(cx, |c, w, cx| {
        let h = hwnd(w);
        let mut pid = 0;
        let mut gui = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let qpc = crate::capture_standby::qpc();
        let (tid, foreground, visible, gui_ok, modifiers) = unsafe {
            let tid = GetWindowThreadProcessId(h, Some(&mut pid));
            (
                tid,
                GetForegroundWindow(),
                IsWindowVisible(h).as_bool(),
                tid != 0 && GetGUIThreadInfo(tid, &mut gui).is_ok(),
                serde_json::json!({
                    "ctrl": GetKeyState(VK_CONTROL.0 as i32) < 0,
                    "alt": GetKeyState(VK_MENU.0 as i32) < 0,
                    "shift": GetKeyState(VK_SHIFT.0 as i32) < 0,
                    "win": GetKeyState(VK_LWIN.0 as i32) < 0 || GetKeyState(VK_RWIN.0 as i32) < 0,
                    "scope": "current UI thread modifier high bits only"
                }),
            )
        };
        serde_json::json!({
            "host": host.name(), "qpc": qpc,
            "hwnd": h.0 as usize, "pid": pid, "tid": tid, "visible": visible,
            "foreground_hwnd": foreground.0 as usize, "own_hwnd_is_foreground": foreground == h,
            "native_gui_thread_query_ok": gui_ok,
            "native_active_hwnd": gui.hwndActive.0 as usize,
            "native_focus_hwnd": gui.hwndFocus.0 as usize,
            "native_capture_hwnd": gui.hwndCapture.0 as usize,
            "gpui_window_active": w.is_window_active(),
            "gpui_logical_text_focus": c.focus.is_focused(w),
            "gpui_any_logical_focus": w.focused(cx).is_some(),
            "gpui_modifiers": format!("{:?}", w.modifiers()), "native_modifiers": modifiers,
            "draft_value": c.editor.text_draft.as_ref().map(|d| d.annotation.text.clone()),
            "selection": [c.input.selection.start, c.input.selection.end],
            "marked": c.input.marked.as_ref().map(|r| [r.start, r.end]),
            "windows_platform_input_handler_registered": "unobserved: no public getter; logical focus does not prove paint registration"
        })
    })
}
async fn reset(host: Host, stats: &Rc<AdapterStats>, cx: &mut AsyncApp) -> Result<()> {
    host.update(cx, |c, w, cx| {
        c.editor.cancel_gesture();
        c.editor.cancel_text();
        c.editor.document = Default::default();
        c.reset_input();
        c.toolbar_panel = ToolbarPanel::None;
        c.editor.selected = None;
        c.perform(Action::Tool(Tool::Text), w, cx);
        let mut style = Style {
            font_size: 28.,
            color: 0x202020,
            ..Default::default()
        };
        style.text.background = false;
        style.text.outline = false;
        c.editor.set_style(style);
        w.focus(&c.focus);
        cx.notify();
    })?;
    install(host.input(cx)?, stats)?;
    settle(cx).await;
    Ok(())
}
async fn begin(host: Host, cx: &mut AsyncApp) -> Result<()> {
    let p = host.point(Point { x: 80., y: 100. }, cx)?;
    host.input(cx)?.drag(p, p);
    settle(cx).await;
    ensure(
        host.update(cx, |c, _, _| c.editor.text_draft.is_some())?,
        "原生点击未建立文字草稿",
    )
}
async fn input(host: Host, text: &str, cx: &mut AsyncApp) -> Result<()> {
    chars(host.input(cx)?, text)?;
    settle(cx).await;
    Ok(())
}
async fn press(
    host: Host,
    vk: VIRTUAL_KEY,
    ctrl: bool,
    shift: bool,
    held: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    key(host.input(cx)?, vk, ctrl, shift, held)?;
    settle(cx).await;
    Ok(())
}
fn confirmed(host: Host, text: &str, cx: &mut AsyncApp) -> Result<()> {
    let s = state(host, cx)?;
    ensure(
        s.draft.is_none()
            && s.marked.is_none()
            && s.items.len() == 1
            && s.items[0].annotation.text == text,
        "文字提交/marked 清理合同不符",
    )
}

async fn own_frame(host: Host, region: [u32; 4], cx: &mut AsyncApp) -> Result<Image> {
    host.update(cx, |_, w, _| w.request_frame_capture(region))??;
    for _ in 0..40 {
        tick(cx).await;
        if let Some(frame) = host.update(cx, |_, w, _| w.take_frame_capture())? {
            let bgra = frame.map_err(|e| e.to_string())?;
            ensure(
                bgra.len() == region[2] as usize * region[3] as usize * 4,
                "自有帧读回长度不符",
            )?;
            return Ok(Image {
                width: region[2],
                height: region[3],
                bgra,
                readback_ms: 0.,
            });
        }
    }
    Err("文字 GPU 帧读回超时".into())
}
#[derive(Debug, PartialEq)]
struct InkBand {
    top: u32,
    bottom: u32,
    left: u32,
    right: u32,
    pixels: u32,
}
fn ink_bands(image: &Image, roi: [u32; 4]) -> Vec<InkBand> {
    // 独立像素扫描，不使用排版器/Annotation.bounds 计算预期行位置。
    let mut bands: Vec<InkBand> = Vec::new();
    for row in 0..roi[3] {
        let mut left = roi[2];
        let mut right = 0;
        let mut count = 0;
        for col in 0..roi[2] {
            let i = (((roi[1] + row) * image.width + roi[0] + col) * 4) as usize;
            if image.bgra[i..i + 3].iter().all(|v| *v < 160) {
                left = left.min(col);
                right = right.max(col);
                count += 1;
            }
        }
        if count < 3 {
            continue;
        }
        if let Some(last) = bands.last_mut().filter(|b| row <= b.bottom + 3) {
            last.bottom = row;
            last.left = last.left.min(left);
            last.right = last.right.max(right);
            last.pixels += count;
        } else {
            bands.push(InkBand {
                top: row,
                bottom: row,
                left,
                right,
                pixels: count,
            });
        }
    }
    bands.retain(|b| b.bottom >= b.top + 7 && b.pixels >= 40);
    bands
}
async fn glyph_pixels(host: Host, cx: &mut AsyncApp) -> Result<serde_json::Value> {
    host.action(Action::Tool(Tool::Edit), cx)?;
    // 正常 hover 离开 ROI，固定悬停状态；光标由系统绘制，不进入 GPU 帧，也不移动系统鼠标。
    let outside_roi = host.point(Point { x: 600., y: 300. }, cx)?;
    host.input(cx)?.hover(outside_roi)?;
    settle(cx).await;
    ensure(
        host.update(cx, |c, w, _| (c.canvas_viewport(w).zoom - 1.).abs() < 0.001)?,
        "两行字形夹具必须保持源像素1×",
    )?;
    let output = host.output(cx)?;
    ensure(
        output.width == 720 && output.height == 420,
        "文字生产导出尺寸不符",
    )?;
    let source_roi = [70, 90, 400, 130];
    let p = host.point(Point { x: 70., y: 90. }, cx)?;
    let gpu = own_frame(host, [p.0, p.1, 400, 130], cx).await?;
    let exported = ink_bands(&output, source_roi);
    let preview = ink_bands(&gpu, [0, 0, 400, 130]);
    // 失败也留下真实产物，不能仅凭一个区间断言猜测坐标或放宽像素 oracle。
    crate::export::save_png(
        output,
        Path::new(FOLDER)
            .join(format!("{}-two-lines-export.png", host.name()))
            .as_path(),
    )?;
    crate::export::save_png(
        gpu,
        Path::new(FOLDER)
            .join(format!("{}-two-lines-gpu.png", host.name()))
            .as_path(),
    )?;
    let geometry = host.update(cx, |c, w, _| {
        format!(
            "host={}, viewport={:?}, crop={:?}, client_roi={:?}, annotations={:?}",
            host.name(),
            c.canvas_viewport(w),
            c.editor.crop,
            [p.0, p.1, 400, 130],
            c.editor.annotations(),
        )
    })?;
    let check = |ok, message: &str| {
        ensure(
            ok,
            &format!("{message}; {geometry}; export={exported:?}; GPU={preview:?}"),
        )
    };
    check(
        exported.len() == 2 && preview.len() == 2,
        "GPU/导出没有两条独立文字行像素",
    )?;
    for bands in [&exported, &preview] {
        check(
            bands[0].top >= 8 && bands[0].top < 45 && bands[0].left >= 8 && bands[0].left < 28,
            "第一行字形未落在独立指定锚点附近",
        )?;
        check(
            bands[1].top > bands[0].bottom + 3
                && bands[1].top < 90
                && bands[1].left >= 8
                && bands[1].left < 28,
            "第二行字形未向下独立换行",
        )?;
    }
    for (a, b) in exported.iter().zip(&preview) {
        for (x, y) in [
            (a.top, b.top),
            (a.bottom, b.bottom),
            (a.left, b.left),
            (a.right, b.right),
        ] {
            check(x.abs_diff(y) <= 2, "GPU与生产导出字形像素位置不同")?;
        }
        check(
            a.pixels.abs_diff(b.pixels) * 3 < a.pixels.max(b.pixels),
            "GPU与生产导出行墨迹覆盖显著不同",
        )?;
    }
    Ok(
        serde_json::json!({"host":host.name(),"roi":source_roi,"export_bands":format!("{exported:?}"),"gpu_bands":format!("{preview:?}"),"oracle":"independent dark-pixel row bands at source anchor (80,100), no shared text-layout geometry"}),
    )
}
async fn native_enter(
    hosts: &[Host],
    stats: &Rc<AdapterStats>,
    groups: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut evidence = Vec::new();
    for &host in hosts {
        reset(host, stats, cx).await?;
        let after_reset = first_char_snapshot(host, cx)?;
        begin(host, cx).await?;
        let before_chars = first_char_snapshot(host, cx)?;
        let char_input = host.input(cx)?;
        let probe = CharProbe::begin(char_input, stats);
        let chars_post_begin_qpc = crate::capture_standby::qpc();
        for unit in FIRST_CHAR_UNITS {
            unsafe {
                PostMessageW(
                    Some(HWND(char_input.handle as *mut _)),
                    WM_CHAR,
                    WPARAM(unit as usize),
                    LPARAM(TAG),
                )?;
            }
        }
        let chars_post_end_qpc = crate::capture_standby::qpc();
        settle(cx).await;
        let (arrivals, unmatched_char_count) = probe.finish();
        let actual = draft_text(host, cx)?;
        if actual != "中文🙂M" {
            println!(
                "{}",
                serde_json::json!({
                    "test": "text-contract-native-first-char-diagnostic", "host": host.name(),
                    "expected": "中文🙂M", "actual": actual,
                    "expected_utf16": "中文🙂M".encode_utf16().collect::<Vec<_>>(),
                    "actual_utf16": actual.encode_utf16().collect::<Vec<_>>(),
                    "chars_post_begin_qpc": chars_post_begin_qpc, "chars_post_end_qpc": chars_post_end_qpc,
                    "wndproc_char_arrivals": arrivals, "arrival_record_limit": 5,
                    "unmatched_char_count": unmatched_char_count, "unmatched_char_values_recorded": false,
                    "private_char_tag": TAG, "expected_units_and_tag_matched_before_recording": true,
                    "after_reset": after_reset, "before_chars": before_chars,
                    "after_original_settle": first_char_snapshot(host, cx)?,
                    "wait_changed": false, "exact_text_assertion_preserved": true,
                    "scope": "explicit smoke first owned synthetic text only; no other user keys, IME context or desktop pixels queried"
                })
            );
        }
        ensure(actual == "中文🙂M", "WM_CHAR 中文/UTF16 emoji 输入不符")?;
        press(host, VK_RETURN, false, true, false, cx).await?;
        input(host, "第二行M", cx).await?;
        ensure(
            draft_text(host, cx)? == "中文🙂M\n第二行M",
            "Shift Enter 未插入 LF",
        )?;
        press(host, VK_RETURN, true, true, false, cx).await?;
        ensure(
            draft_text(host, cx)? == "中文🙂M\n第二行M\n" && state(host, cx)?.items.is_empty(),
            "Ctrl Shift Enter 提前提交/未插入 LF",
        )?;
        press(host, VK_BACK, false, false, false, cx).await?;
        ensure(
            draft_text(host, cx)? == "中文🙂M\n第二行M",
            "Backspace 未去掉最后一个 LF",
        )?;
        press(host, VK_RETURN, false, false, false, cx).await?;
        confirmed(host, "中文🙂M\n第二行M", cx)?;
        host.action(Action::Undo, cx)?;
        let s = state(host, cx)?;
        ensure(
            s.items.is_empty() && !s.undo && s.redo,
            "两行文字不是一次撤销事务",
        )?;
        host.action(Action::Redo, cx)?;
        confirmed(host, "中文🙂M\n第二行M", cx)?;
        evidence.push(glyph_pixels(host, cx).await?);
        reset(host, stats, cx).await?;
        begin(host, cx).await?;
        input(host, "Ctrl确认🙂", cx).await?;
        press(host, VK_RETURN, true, false, false, cx).await?;
        confirmed(host, "Ctrl确认🙂", cx)?;
        host.action(Action::Undo, cx)?;
        ensure(
            state(host, cx)?.items.is_empty() && !state(host, cx)?.undo,
            "Ctrl Enter 提交不止一次历史",
        )?;
    }
    groups.push(serde_json::json!({"group":"native-text-enter-modifiers-unicode-gpu-export","ok":true,"hosts":evidence,"native_keys":["Enter","Ctrl Enter","Shift Enter","Ctrl Shift Enter"],"unicode_route":"WM_CHAR UTF16 incl surrogate pair"}));
    Ok(())
}
async fn edit(host: Host, cx: &mut AsyncApp) -> Result<()> {
    host.update(cx, |c, _, _| {
        c.editor.selected = c.editor.document.items().first().map(|o| o.id);
    })?;
    host.action(Action::EditText, cx)?;
    settle(cx).await;
    ensure(
        state(host, cx)?.draft.is_some(),
        "生产 EditText 未重新编辑文字",
    )?;
    press(host, VIRTUAL_KEY(0x41), true, false, false, cx).await?;
    Ok(())
}
async fn native_escape(
    hosts: &[Host],
    stats: &Rc<AdapterStats>,
    groups: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    for &host in hosts {
        reset(host, stats, cx).await?;
        begin(host, cx).await?;
        input(host, "原始M", cx).await?;
        press(host, VK_RETURN, false, false, false, cx).await?;
        confirmed(host, "原始M", cx)?;
        edit(host, cx).await?;
        input(host, "改字中文🙂M", cx).await?;
        press(host, VK_ESCAPE, false, false, false, cx).await?;
        confirmed(host, "改字中文🙂M", cx)?;
        host.action(Action::Undo, cx)?;
        confirmed(host, "原始M", cx)?;
        host.action(Action::Redo, cx)?;
        confirmed(host, "改字中文🙂M", cx)?;
        edit(host, cx).await?;
        input(host, "   ", cx).await?;
        press(host, VK_ESCAPE, false, false, false, cx).await?;
        ensure(
            state(host, cx)?.items.is_empty() && state(host, cx)?.draft.is_none(),
            "Esc 空白编辑没有删除旧文字",
        )?;
        host.action(Action::Undo, cx)?;
        confirmed(host, "改字中文🙂M", cx)?;
        host.action(Action::Redo, cx)?;
        ensure(
            state(host, cx)?.items.is_empty(),
            "空白删除 redo 未恢复删除",
        )?;
        // 空对象删除与空白新草稿都不产生新 command，也不能清旧 redo。
        for terminal in [VK_RETURN, VK_ESCAPE, VK_BACK, VK_DELETE] {
            reset(host, stats, cx).await?;
            begin(host, cx).await?;
            if terminal == VK_RETURN || terminal == VK_ESCAPE {
                input(host, "   ", cx).await?;
            }
            press(host, terminal, false, false, false, cx).await?;
            let s = state(host, cx)?;
            ensure(
                s.draft.is_none() && s.items.is_empty() && !s.undo && !s.redo,
                "空白/空文字终结意外写入历史",
            )?;
        }
        // 已有对象编辑为空后 Backspace 和 Delete 都只删除一次。
        for terminal in [VK_BACK, VK_DELETE] {
            reset(host, stats, cx).await?;
            begin(host, cx).await?;
            input(host, "保留M", cx).await?;
            press(host, VK_RETURN, false, false, false, cx).await?;
            edit(host, cx).await?;
            input(host, "", cx).await?;
            // WM_CHAR 空串不替换；第一次 Backspace 清选中文字，第二次结束空草稿。
            press(host, VK_BACK, false, false, false, cx).await?;
            ensure(draft_text(host, cx)?.is_empty(), "清空选中文字失败")?;
            press(host, terminal, false, false, false, cx).await?;
            ensure(
                state(host, cx)?.draft.is_none() && state(host, cx)?.items.is_empty(),
                "空 Backspace/Delete 未删除旧对象",
            )?;
            host.action(Action::Undo, cx)?;
            confirmed(host, "保留M", cx)?;
        }
    }
    groups.push(serde_json::json!({"group":"native-text-escape-edit-empty-undo","ok":true,"hosts":["image_editor","pin"],"cases":["Esc commits edited unicode","Esc whitespace deletes once","new blank has no history","empty Backspace/Delete removes existing once"]}));
    Ok(())
}
async fn native_held(
    hosts: &[Host],
    stats: &Rc<AdapterStats>,
    groups: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    for &host in hosts {
        reset(host, stats, cx).await?;
        begin(host, cx).await?;
        input(host, "保持🙂M", cx).await?;
        let before = state(host, cx)?;
        let chrome = host.chrome(cx)?;
        for vk in [VK_RETURN, VK_ESCAPE] {
            press(host, vk, false, false, true, cx).await?;
            ensure(
                state(host, cx)? == before && host.chrome(cx)? == chrome,
                "held terminal 改变文字/输出/Chrome",
            )?;
        }
        press(host, VK_RETURN, false, false, false, cx).await?;
        confirmed(host, "保持🙂M", cx)?;
        let committed = state(host, cx)?;
        for vk in [VK_RETURN, VK_ESCAPE] {
            press(host, vk, false, false, true, cx).await?;
            ensure(
                state(host, cx)? == committed && host.chrome(cx)? == chrome,
                "文字结束后的 held terminal 泄漏到宿主 Copy/Close/Chrome",
            )?;
        }
        ensure(
            unsafe { IsWindow(Some(HWND(host.input(cx)?.handle as *mut _))).as_bool() },
            "held Escape 关闭宿主 HWND",
        )?;
    }
    groups.push(serde_json::json!({"group":"native-text-terminal-held-no-host-actions","ok":true,"hosts":["image_editor","pin"],"lparam_bit30":true,"non_draft_keys":"held only; no fresh Enter, no clipboard access"}));
    Ok(())
}

async fn property(
    host: Host,
    id: &'static str,
    stats: &Rc<AdapterStats>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut point = None;
    for _ in 0..20 {
        point = host.update(cx, |c, _, _| c.ui_positions.borrow().get(id).copied())?;
        if point.is_some() {
            break;
        }
        tick(cx).await;
    }
    let point = point.ok_or_else(|| format!("普通文字参数控件未绘制：{id}"))?;
    let target = host.property_input(cx)?;
    install(target, stats)?;
    target.click(point);
    settle(cx).await;
    Ok(())
}
fn same_body(before: &State, after: &State) -> bool {
    let text = |s: &State| {
        s.draft
            .as_ref()
            .map(|(id, a)| (*id, a.tool, a.text.clone(), a.points.clone()))
    };
    text(before) == text(after)
        && before.items == after.items
        && before.revision == after.revision
        && before.edit_revision == after.edit_revision
        && before.undo == after.undo
        && before.redo == after.redo
        && before.tool == after.tool
        && before.marked == after.marked
        && before.selection == after.selection
}
fn output_stamp() -> Result<Vec<(std::path::PathBuf, u64, std::time::SystemTime)>> {
    let folder = Path::new(FOLDER).join("output");
    if !folder.exists() {
        return Ok(Vec::new());
    }
    let mut stamp = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        stamp.push((entry.path(), metadata.len(), metadata.modified()?));
    }
    stamp.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(stamp)
}
async fn parameter_return(
    hosts: &[Host],
    stats: &Rc<AdapterStats>,
    groups: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut evidence = Vec::new();
    for &host in hosts {
        reset(host, stats, cx).await?;
        begin(host, cx).await?;
        input(host, "正文中文🙂M", cx).await?;
        let before = state(host, cx)?;
        let chrome = host.chrome(cx)?;
        property(host, "text-background-menu", stats, cx).await?;
        ensure(
            host.update(cx, |c, _, _| c.toolbar_panel == ToolbarPanel::TextBackgroundStyle)?,
            "文字格式面板未打开",
        )?;
        property(host, "text-background-radius", stats, cx).await?;
        ensure(
            host.update(cx, |c, _, _| {
                c.toolbar_panel == ToolbarPanel::TextBackgroundRadius
            })?,
            "圆角子参数未打开",
        )?;
        let numeric = host.property_input(cx)?;
        install(numeric, stats)?;
        chars(numeric, "17")?;
        settle(cx).await;
        key(numeric, VK_RETURN, false, false, true)?;
        settle(cx).await;
        ensure(
            host.update(cx, |c, _, _| {
                c.toolbar_panel == ToolbarPanel::TextBackgroundRadius
            })? && same_body(&before, &state(host, cx)?),
            "held 参数 Enter 提前确认/结束正文",
        )?;
        key(numeric, VK_RETURN, false, false, false)?;
        settle(cx).await;
        ensure(
            host.update(cx, |c, _, _| {
                c.toolbar_panel == ToolbarPanel::TextBackgroundStyle
                    && c.editor.style.text.background_radius == 17.
            })?,
            "参数 Enter 未确认17/回格式页",
        )?;
        let applied = state(host, cx)?;
        ensure(
            same_body(&before, &applied)
                && applied
                    .draft
                    .as_ref()
                    .is_some_and(|(_, a)| a.style.text.background_radius == 17.),
            "参数确认结束草稿/改动历史/没有更新正文样式",
        )?;
        property(host, "text-background-radius", stats, cx).await?;
        let numeric = host.property_input(cx)?;
        install(numeric, stats)?;
        chars(numeric, "23")?;
        settle(cx).await;
        key(numeric, VK_ESCAPE, false, false, true)?;
        settle(cx).await;
        ensure(
            host.update(cx, |c, _, _| {
                c.toolbar_panel == ToolbarPanel::TextBackgroundRadius
            })? && state(host, cx)? == applied,
            "held 参数 Esc 返回页面/改变草稿",
        )?;
        key(numeric, VK_ESCAPE, false, false, false)?;
        settle(cx).await;
        ensure(
            host.update(cx, |c, _, _| c.toolbar_panel == ToolbarPanel::TextBackgroundStyle)?
                && state(host, cx)? == applied,
            "参数 Esc 未只返回格式页",
        )?;
        let parent = host.property_input(cx)?;
        install(parent, stats)?;
        key(parent, VK_ESCAPE, false, false, true)?;
        settle(cx).await;
        ensure(
            host.update(cx, |c, _, _| c.toolbar_panel == ToolbarPanel::TextBackgroundStyle)?
                && state(host, cx)? == applied,
            "返回后的 held Esc 穿透父页",
        )?;
        key(parent, VK_ESCAPE, false, false, false)?;
        settle(cx).await;
        ensure(
            host.update(cx, |c, _, _| c.toolbar_panel == ToolbarPanel::None)?
                && state(host, cx)? == applied,
            "格式页返回结束正文",
        )?;
        // 不手工恢复逻辑焦点：验证生产返回过程让 WM_CHAR 回到正文。
        input(host, "续M", cx).await?;
        ensure(
            draft_text(host, cx)? == "正文中文🙂M续M"
                && state(host, cx)?.items.is_empty()
                && host.chrome(cx)? == chrome,
            "参数返回未恢复正文原生输入/意外提交或收栏",
        )?;
        evidence.push(serde_json::json!({"host":host.name(),"numeric_hwnd":numeric.handle,"parent_hwnd":parent.handle,"body_after":"正文中文🙂M续M","radius":17,"fresh_parameter_enter":true,"held_enter_escape":true,"focus_restored_by_production":true}));
    }
    groups.push(serde_json::json!({"group":"native-text-parameter-return-keeps-draft","ok":true,"hosts":evidence}));
    Ok(())
}
async fn ime_adapter(
    hosts: &[Host],
    stats: &Rc<AdapterStats>,
    groups: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    for &host in hosts {
        reset(host, stats, cx).await?;
        let output_before = output_stamp()?;
        begin(host, cx).await?;
        let candidate = "候选中🙂";
        let utf = candidate.encode_utf16().count();
        host.update(cx, |c, w, cx| {
            c.replace_and_mark_text_in_range(None, candidate, Some(utf..utf), w, cx);
            let marked = c.marked_text_range(w, cx);
            let caret = c.selected_text_range(false, w, cx);
            ensure(
                marked == Some(0..utf) && caret.is_some_and(|s| s.range == (utf..utf)),
                "生产 EntityInputHandler marked/UTF16 光标合同错误",
            )
        })??;
        settle(cx).await;
        let before = state(host, cx)?;
        let chrome = host.chrome(cx)?;
        for (vk, ctrl, shift, held) in [
            (VK_RETURN, false, false, false),
            (VK_RETURN, true, false, false),
            (VK_RETURN, false, true, false),
            (VK_ESCAPE, false, false, false),
            (VK_RETURN, false, false, true),
        ] {
            press(host, vk, ctrl, shift, held, cx).await?;
            ensure(
                state(host, cx)? == before && host.chrome(cx)? == chrome,
                "组合态原生 KEYDOWN 未旁路正文/宿主命令",
            )?;
        }
        ensure(
            host.update(cx, |c, _, _| c.try_finish_content().is_err())?,
            "组合态 try_finish_content 没有拒绝",
        )?;
        for action in [
            Action::Tool(Tool::Rectangle),
            Action::QuickSave,
            Action::Reselect,
        ] {
            host.action(action, cx)?;
            settle(cx).await;
            ensure(
                state(host, cx)? == before && host.chrome(cx)? == chrome,
                "组合态切工具/输出/重选破坏文字或历史",
            )?;
        }
        if let Host::Pin(h) = host {
            ensure(
                h.update(cx, |v, _, cx| v.user_export_snapshot(cx).is_err())
                    .map_err(|e| e.to_string())?,
                "Pin 生产输出没有组合态门",
            )?;
            ensure(state(host, cx)? == before, "Pin 输出失败修改 marked draft")?;
        }
        ensure(
            output_stamp()? == output_before,
            "组合态 QuickSave 意外新增/改写文件",
        )?;
        host.action(Action::Undo, cx)?;
        settle(cx).await;
        let undone = state(host, cx)?;
        ensure(
            undone.draft.is_none()
                && undone.marked.is_none()
                && undone.items.is_empty()
                && !undone.undo
                && !undone.redo,
            "显式 Undo 未回滚组合草稿/清 marked",
        )?;
        begin(host, cx).await?;
        host.update(cx, |c, w, cx| {
            c.replace_and_mark_text_in_range(None, "恢复候选🙂", None, w, cx);
            c.replace_text_in_range(None, "确认中文🙂M", w, cx);
        })?;
        settle(cx).await;
        ensure(
            draft_text(host, cx)? == "确认中文🙂M" && state(host, cx)?.marked.is_none(),
            "生产确认适配器未替换 marked 或清理组合态",
        )?;
        press(host, VK_RETURN, false, false, false, cx).await?;
        confirmed(host, "确认中文🙂M", cx)?;
        host.action(Action::Undo, cx)?;
        ensure(
            state(host, cx)?.items.is_empty() && !state(host, cx)?.undo,
            "组合确认提交不是一次历史",
        )?;
    }
    groups.push(serde_json::json!({"group":"native-text-ime-adapter-native-key-bypass","ok":true,"hosts":["image_editor","pin"],"production_entity_input_handler":true,"windows_composition_keydown_bypass":true,"gates":["try_finish_content","Tool","QuickSave","Reselect","Pin user_export_snapshot"],"explicit_undo_clears_marked":true,"real_candidate_ui_tested":false,"scope":"adapter marked/confirmed plus real own HWND WndProc; no IME candidate UI automation"}));
    Ok(())
}

pub(crate) fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    *errors.borrow_mut() = Some("普通文字合同专项未完成".into());
    let stats = Rc::new(AdapterStats::default());
    let requested = match requested_groups() {
        Ok(groups) => groups,
        Err(error) => {
            *errors.borrow_mut() = Some(error.to_string());
            cx.quit();
            return;
        }
    };
    let setup = (|| -> Result<Vec<Host>> {
        std::fs::create_dir_all(FOLDER)?;
        ensure(
            crate::preferences::data_dir()
                == std::env::current_dir()?.join(FOLDER).join("settings"),
            "普通文字专项设置未隔离",
        )?;
        crate::preferences::Preferences {
            history: false,
            pin_restore: false,
            ..Default::default()
        }
        .save()?;
        std::fs::write(
            crate::preferences::data_dir().join("tool-presets.json"),
            "{}\n",
        )?;
        let mut store = crate::save_store::SaveStore::load();
        store.config = crate::save_options::SaveConfig::default();
        store.config.directory = std::env::current_dir()?.join(FOLDER).join("output");
        store.save()?;
        let image = crate::image_editor::open(fixture(), None, cx)?;
        let pin = PinView::open_state(fixture(), (150, 120), PinState::default(), cx)?;
        pin.update(cx, |v, w, cx| -> Result<()> {
            v.ensure_canvas(w, cx)?;
            v.perform(PinAction::Chrome, w, cx);
            v.annotation(Action::Tool(Tool::Text), w, cx);
            Ok(())
        })??;
        Ok(vec![Host::Image(image), Host::Pin(pin)])
    })();
    let hosts = match setup {
        Ok(hosts) => hosts,
        Err(error) => {
            *errors.borrow_mut() = Some(error.to_string());
            cx.quit();
            return;
        }
    };
    cx.spawn(async move|cx| {
        let mut groups=Vec::new();
        let result=async {
            settle(cx).await;
            if requested.contains(&GROUPS[0].1) { native_enter(&hosts,&stats,&mut groups,cx).await?; }
            if requested.contains(&GROUPS[1].1) { native_escape(&hosts,&stats,&mut groups,cx).await?; }
            if requested.contains(&GROUPS[2].1) { native_held(&hosts,&stats,&mut groups,cx).await?; }
            if requested.contains(&GROUPS[3].1) { parameter_return(&hosts,&stats,&mut groups,cx).await?; }
            if requested.contains(&GROUPS[4].1) { ime_adapter(&hosts,&stats,&mut groups,cx).await?; }
            ensure(stats.keys.get()>0&&stats.keys.get()==stats.restored.get()&&stats.errors.get()==0,"UI 线程键盘状态 adapter 未完整保存恢复")?;
            Ok::<(),Box<dyn std::error::Error>>(())
        }.await;
        let error=result.err().map(|e|e.to_string());
        let skipped:Vec<_>=GROUPS.iter().map(|(_,full)|*full).filter(|full|!requested.contains(full)).collect();
        let report=serde_json::json!({"test":"text-contract-native","ok":error.is_none(),"error":error,"groups":groups,"requested_groups":requested,"skipped_groups":skipped,"keyboard_adapter":{"scope":"tagged own HWND WM_KEYDOWN only; UI thread GetKeyboardState/SetKeyboardState saved/restored, no SendInput","keys":stats.keys.get(),"restored":stats.restored.get(),"errors":stats.errors.get()},"ime_adapter_only":true,"real_candidate_ui_tested":false,"watermark_single_line_tested":false,"clipboard_read":false,"clipboard_written":false,"desktop_read":false,"global_pointer_moved":false,"global_input_injected":false,"source":"owned synthetic ImageEditor and Pin; posted WM_CHAR/WM_KEYDOWN; production EntityInputHandler, own GPU framebuffer, production exports"});
        let write=std::fs::write(Path::new(FOLDER).join("report.json"),serde_json::to_vec_pretty(&report).unwrap_or_default());
        *errors.borrow_mut()=error.or_else(||write.err().map(|e|e.to_string()));println!("{report}");
        for host in hosts {host.remove(cx);}
        let _=cx.update(|cx|cx.quit());
    }).detach();
}
