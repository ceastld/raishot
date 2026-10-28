//! 自有经典控件的真实 UIA + GPUI 专项；不改系统指针，不读写剪贴板或保存图像。
use crate::{
    annotation::Tool,
    canvas_source::CanvasSource,
    capture_view::CaptureView,
    control_selection::{self, ControlPathState},
    fast_capture::{FrozenDesktop, FrozenScreen},
    geometry::{Rect, ScreenRect},
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    selection_assist::WindowTarget,
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{App, AsyncApp, Entity, RenderImage, WeakEntity, Window};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    cell::RefCell,
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use windows::{
    core::{w, PWSTR},
    Win32::{
        Foundation::*,
        Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL},
        System::Threading::*,
        UI::{
            HiDpi::{
                SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            },
            Input::KeyboardAndMouse::{
                GetActiveWindow, GetCapture, GetLastInputInfo, LASTINPUTINFO,
            },
            WindowsAndMessaging::*,
        },
    },
};

const SUITE: &str = "control-selection";
const POLL_LIMIT: Duration = Duration::from_secs(8);
const INPUT_CHANGED: &str = "控件专项收到新的用户输入，保留窗口";

#[derive(Clone, Deserialize)]
struct Ready {
    schema: u32,
    suite: String,
    nonce: String,
    ok: bool,
    identity: FixtureIdentity,
    token_property_name: String,
    command_directory: String,
    move_command_filename: String,
    move_ack_filename: String,
    input_baseline_tick: u32,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
struct FixtureIdentity {
    pid: u32,
    creation_filetime: String,
    image: String,
    hwnd: String,
    thread_id: u32,
    thread_creation_filetime: String,
    token: String,
    class_name: String,
    controls: Vec<Control>,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
struct Control {
    id: String,
    hwnd: String,
    class_name: String,
    pid: u32,
    tid: u32,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    synthetic_text_only: bool,
    expected_text: String,
    password_should_not_be_returned: bool,
}
#[derive(Deserialize)]
struct MoveAck {
    schema: u32,
    nonce: String,
    sequence: u32,
    op: String,
    ok: bool,
    before: FixtureIdentity,
    after: FixtureIdentity,
    own_thread_command_acknowledged: bool,
    input_tick_unchanged: bool,
    product_started: bool,
}
struct OwnedHandle(Option<usize>);
impl OwnedHandle {
    fn raw(&self) -> Result<HANDLE> {
        Ok(HANDLE(self.0.ok_or("专项原查询句柄已关闭")? as *mut _))
    }
    fn close(&mut self) -> Result<()> {
        if let Some(raw) = self.0 {
            unsafe { CloseHandle(HANDLE(raw as *mut _))? };
            self.0 = None;
        }
        Ok(())
    }
}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
struct Fixture {
    ready: Ready,
    ready_path: PathBuf,
    process: OwnedHandle,
    thread: OwnedHandle,
}

fn number(value: &str) -> Result<u64> {
    let value = value.parse::<u64>()?;
    if value == 0 {
        return Err("专项身份数字必须非零".into());
    }
    Ok(value)
}
fn filetime(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}
fn identity_handle(value: &str) -> Result<HWND> {
    Ok(HWND(usize::try_from(number(value)?)? as *mut _))
}
fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn emit(test: &str, details: serde_json::Value) {
    crate::startup_probe::emit_trace_value(
        &json!({"case":SUITE,"test":test,"ok":true,"details":details}),
    );
}
fn report_identity(identity: &FixtureIdentity) -> serde_json::Value {
    json!({"pid":identity.pid,"creation_filetime":identity.creation_filetime,"image":identity.image,
        "hwnd":identity.hwnd,"thread_id":identity.thread_id,"thread_creation_filetime":identity.thread_creation_filetime,
        "token":identity.token,"class_name":identity.class_name,"controls":identity.controls.iter().map(|control|
            json!({"id":control.id,"hwnd":control.hwnd,"class_name":control.class_name,"pid":control.pid,"tid":control.tid,
                "left":control.left,"top":control.top,"width":control.width,"height":control.height,
                "synthetic_text_only":control.synthetic_text_only,"expected_text_omitted":true})).collect::<Vec<_>>()})
}
fn input_tick() -> Result<u32> {
    let mut input = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetLastInputInfo(&mut input).ok()?;
    }
    Ok(input.dwTime)
}
fn input_unchanged(baseline: u32) -> Result<()> {
    require(input_tick()? == baseline, INPUT_CHANGED)
}

struct PhysicalScope(Option<DPI_AWARENESS_CONTEXT>);
impl PhysicalScope {
    fn new() -> Result<Self> {
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        require(!previous.0.is_null(), "专项物理 DPI scope 失败")?;
        Ok(Self(Some(previous)))
    }
    fn restore(&mut self) -> Result<()> {
        if let Some(previous) = self.0 {
            require(
                !unsafe { SetThreadDpiAwarenessContext(previous) }
                    .0
                    .is_null(),
                "专项物理 DPI restore 失败",
            )?;
            self.0 = None;
        }
        Ok(())
    }
}
impl Drop for PhysicalScope {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

impl Fixture {
    fn bind() -> Result<Self> {
        let ready_path = PathBuf::from(std::env::var("RUSTSHOT_UIA_FIXTURE_READY")?);
        require(ready_path.is_absolute(), "fixture Ready 必须是绝对路径")?;
        let parent = ready_path
            .parent()
            .ok_or("fixture Ready 无父目录")?
            .canonicalize()?;
        let qa = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa")
            .canonicalize()?;
        require(
            parent.starts_with(qa),
            "fixture Ready 必须属于本项目 target/qa",
        )?;
        let bytes = std::fs::read(&ready_path)?;
        require(bytes.len() <= 32_768, "fixture Ready 超出大小上限")?;
        let ready: Ready = serde_json::from_slice(&bytes)?;
        require(
            ready.schema == 1
                && ready.suite == "owned-control-selection-fixture"
                && ready.ok
                && !ready.nonce.is_empty()
                && ready.nonce.len() <= 128,
            "fixture Ready 协议不符",
        )?;
        require(
            ready.token_property_name == "RustShotOwnedUiaFixtureToken"
                && PathBuf::from(&ready.command_directory).canonicalize()? == parent
                && ready.move_command_filename == "command-0001.json"
                && ready.move_ack_filename == "ack-0001.json",
            "fixture nonce 命令路径不符",
        )?;
        let identity = &ready.identity;
        require(
            identity.pid > 0
                && identity.pid != std::process::id()
                && identity.thread_id > 0
                && identity.controls.len() == 4,
            "fixture 必须是另一自有进程的四个经典控件",
        )?;
        for id in ["window", "button", "edit", "password-edit"] {
            require(
                identity
                    .controls
                    .iter()
                    .filter(|control| control.id == id)
                    .count()
                    == 1,
                "fixture 控件 ID 不唯一",
            )?;
        }
        require(
            identity.controls.iter().all(|control| {
                control.password_should_not_be_returned == (control.id == "password-edit")
            }),
            "fixture 密码控件合同不符",
        )?;
        require(
            identity.controls.iter().all(|control| {
                control.synthetic_text_only
                    && control.pid == identity.pid
                    && control.tid == identity.thread_id
                    && control.width > 0
                    && control.height > 0
            }),
            "fixture 控件不是同一自有 pumping thread 的合成内容",
        )?;
        let process = OwnedHandle(Some(
            unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    identity.pid,
                )?
            }
            .0 as usize,
        ));
        let thread = OwnedHandle(Some(
            unsafe {
                OpenThread(
                    THREAD_QUERY_LIMITED_INFORMATION | THREAD_SYNCHRONIZE,
                    false,
                    identity.thread_id,
                )?
            }
            .0 as usize,
        ));
        let fixture = Self {
            ready,
            ready_path,
            process,
            thread,
        };
        fixture.check(&fixture.ready.identity)?;
        Ok(fixture)
    }
    fn control(&self, id: &str) -> Result<&Control> {
        self.ready
            .identity
            .controls
            .iter()
            .find(|control| control.id == id)
            .ok_or_else(|| format!("fixture 缺少 {id}").into())
    }
    fn check(&self, expected: &FixtureIdentity) -> Result<()> {
        let mut scope = PhysicalScope::new()?;
        let result = self.check_physical(expected);
        scope.restore()?;
        result
    }
    fn check_physical(&self, expected: &FixtureIdentity) -> Result<()> {
        let original = &self.ready.identity;
        require(
            expected.pid == original.pid
                && expected.creation_filetime == original.creation_filetime
                && expected.image == original.image
                && expected.hwnd == original.hwnd
                && expected.thread_id == original.thread_id
                && expected.thread_creation_filetime == original.thread_creation_filetime
                && expected.token == original.token
                && expected.class_name == original.class_name,
            "fixture 更新替换了原进程/线程/窗口身份",
        )?;
        let process = self.process.raw()?;
        let thread = self.thread.raw()?;
        unsafe {
            require(
                WaitForSingleObject(process, 0) == WAIT_TIMEOUT
                    && WaitForSingleObject(thread, 0) == WAIT_TIMEOUT,
                "fixture 原进程或线程不再存活",
            )?;
            require(
                GetProcessId(process) == original.pid
                    && GetThreadId(thread) == original.thread_id
                    && GetProcessIdOfThread(thread) == original.pid,
                "fixture 原查询 HANDLE 身份不符",
            )?;
            let mut created = FILETIME::default();
            let mut exit = FILETIME::default();
            let mut kernel = FILETIME::default();
            let mut user = FILETIME::default();
            GetProcessTimes(process, &mut created, &mut exit, &mut kernel, &mut user)?;
            require(
                filetime(created) == number(&original.creation_filetime)?,
                "fixture 原进程 FILETIME 不符",
            )?;
            GetThreadTimes(thread, &mut created, &mut exit, &mut kernel, &mut user)?;
            require(
                filetime(created) == number(&original.thread_creation_filetime)?,
                "fixture 原线程 FILETIME 不符",
            )?;
            let mut image = [0_u16; 4096];
            let mut count = image.len() as u32;
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(image.as_mut_ptr()),
                &mut count,
            )?;
            require(
                String::from_utf16(&image[..count as usize])?.eq_ignore_ascii_case(&original.image),
                "fixture 原 image path 不符",
            )?;
        }
        let host = identity_handle(&original.hwnd)?;
        let token = usize::try_from(number(&original.token)?)?;
        for control in &expected.controls {
            let old = original
                .controls
                .iter()
                .find(|old| old.id == control.id)
                .ok_or("fixture 更新出现未知控件")?;
            require(
                control.hwnd == old.hwnd
                    && control.class_name == old.class_name
                    && control.pid == old.pid
                    && control.tid == old.tid
                    && control.expected_text == old.expected_text
                    && control.synthetic_text_only
                    && control.password_should_not_be_returned
                        == old.password_should_not_be_returned,
                "fixture 更新替换了原控件",
            )?;
            let handle = identity_handle(&control.hwnd)?;
            let mut pid = 0;
            let mut bounds = RECT::default();
            let mut class = [0_u16; 256];
            unsafe {
                require(
                    IsWindow(Some(handle)).as_bool() && IsWindowVisible(handle).as_bool(),
                    "fixture 控件 HWND 不再可见",
                )?;
                require(
                    GetWindowThreadProcessId(handle, Some(&mut pid)) == original.thread_id
                        && pid == original.pid,
                    "fixture 控件 HWND 的 PID/TID 不符",
                )?;
                require(
                    GetPropW(handle, w!("RustShotOwnedUiaFixtureToken")).0 as usize == token,
                    "fixture 控件 nonce token 不符",
                )?;
                let count = GetClassNameW(handle, &mut class);
                require(
                    count > 0
                        && String::from_utf16(&class[..count as usize])?
                            .eq_ignore_ascii_case(&control.class_name),
                    "fixture 控件 class 不符",
                )?;
                GetWindowRect(handle, &mut bounds)?;
                require(
                    bounds.left == control.left
                        && bounds.top == control.top
                        && i64::from(bounds.right) - i64::from(bounds.left)
                            == i64::from(control.width)
                        && i64::from(bounds.bottom) - i64::from(bounds.top)
                            == i64::from(control.height),
                    "fixture 控件物理矩形不符",
                )?;
                if control.id != "window" {
                    require(
                        GetParent(handle)? == host,
                        "fixture 控件不是原 host 的直接子窗口",
                    )?;
                }
            }
        }
        require(expected.controls.len() == 4, "fixture 更新控件数变化")?;
        Ok(())
    }
    fn target(&self, canvas: ScreenRect) -> Result<WindowTarget> {
        let control = self.control("window")?;
        require(
            control.hwnd == self.ready.identity.hwnd
                && control.class_name == self.ready.identity.class_name,
            "fixture window 与 host 不一致",
        )?;
        let mut target = WindowTarget {
            rect: local_rect(canvas, control)?,
            title: control.expected_text.clone(),
            global_bounds: RECT {
                left: control.left,
                top: control.top,
                right: control
                    .left
                    .checked_add(control.width)
                    .ok_or("fixture right 越界")?,
                bottom: control
                    .top
                    .checked_add(control.height)
                    .ok_or("fixture bottom 越界")?,
            },
            hwnd: identity_handle(&control.hwnd)?.0 as usize,
            process_id: control.pid,
            thread_id: control.tid,
        };
        let bounds = crate::selection_assist::visible_window_bounds(
            HWND(target.hwnd as *mut _),
            target.global_bounds,
        );
        let mut visible = control.clone();
        visible.left = bounds.left;
        visible.top = bounds.top;
        visible.width = bounds.right - bounds.left;
        visible.height = bounds.bottom - bounds.top;
        target.rect = local_rect(canvas, &visible)?;
        Ok(target)
    }
    fn close_queries(&mut self) -> Result<()> {
        self.thread.close()?;
        self.process.close()
    }
}

fn local_rect(canvas: ScreenRect, control: &Control) -> Result<Rect> {
    let x = i64::from(control.left) - i64::from(canvas.left);
    let y = i64::from(control.top) - i64::from(canvas.top);
    require(
        x >= 0 && y >= 0 && control.width > 0 && control.height > 0,
        "fixture 不完整位于冻结画布",
    )?;
    let rect = Rect {
        x: u32::try_from(x)?,
        y: u32::try_from(y)?,
        width: control.width as u32,
        height: control.height as u32,
    };
    require(
        rect.valid(canvas.width, canvas.height),
        "fixture 控件超出画布",
    )?;
    Ok(rect)
}
fn center(control: &Control) -> Result<(i32, i32)> {
    Ok((
        control
            .left
            .checked_add(control.width / 2)
            .ok_or("fixture x 溢出")?,
        control
            .top
            .checked_add(control.height / 2)
            .ok_or("fixture y 溢出")?,
    ))
}

fn verify_backend(fixture: &Fixture, canvas: ScreenRect, baseline: u32) -> Result<Rect> {
    input_unchanged(baseline)?;
    fixture.check(&fixture.ready.identity)?;
    let target = fixture.target(canvas)?;
    let button = fixture.control("button")?;
    let path = control_selection::probe(canvas, target.clone(), center(button)?)?;
    require(
        path.point == center(button)?
            && path.state != ControlPathState::WindowOnly
            && path.targets.first().is_some_and(|control| {
                control.rect == local_rect(canvas, button).unwrap_or(Rect::full(0, 0))
            })
            && path
                .targets
                .last()
                .is_some_and(|control| control.rect == target.rect)
            && path.targets.len() >= 2,
        "真实 BUTTON child→ancestor 路径不符",
    )?;
    let parent = path.targets.get(1).ok_or("真实 BUTTON 缺少父层级")?.rect;
    emit(
        "uia_button_child_to_window",
        json!({"targets":path.targets.len(),"fixture_pid":fixture.ready.identity.pid}),
    );
    let edit = fixture.control("edit")?;
    require(
        edit.expected_text == "UIA fixture editable text 123",
        "fixture EDIT 合成正文不符",
    )?;
    require(
        control_selection::text(canvas, vec![target.clone()], local_rect(canvas, edit)?)?
            == Some(edit.expected_text.clone()),
        "真实 EDIT exact BBox 正文不符",
    )?;
    emit(
        "uia_edit_exact_text",
        json!({"synthetic_only":true,"clipboard_accessed":false}),
    );
    let password = fixture.control("password-edit")?;
    require(
        control_selection::text(canvas, vec![target.clone()], local_rect(canvas, password)?)?
            .is_none(),
        "密码控件没有拒绝文字读取",
    )?;
    emit(
        "uia_password_rejected",
        json!({"password_content_logged":false}),
    );
    let mut partial = local_rect(canvas, edit)?;
    partial.width = (partial.width / 2).max(1);
    require(
        control_selection::text(canvas, vec![target.clone()], partial)?.is_none(),
        "局部 EDIT ROI 未回退 OCR",
    )?;
    emit("uia_partial_roi_fallback", json!({"ocr_not_invoked":true}));
    let mut forged = target.clone();
    forged.process_id = forged.process_id.checked_add(1).ok_or("伪造 PID 溢出")?;
    require(
        control_selection::probe(canvas, forged, center(button)?).is_err(),
        "伪造 PID 被接受",
    )?;
    let mut forged = target;
    forged.thread_id = forged.thread_id.checked_add(1).ok_or("伪造 TID 溢出")?;
    require(
        control_selection::probe(canvas, forged, center(button)?).is_err(),
        "伪造 TID 被接受",
    )?;
    emit("uia_forged_pid_tid_rejected", json!({"attempts":2}));
    fixture.check(&fixture.ready.identity)?;
    input_unchanged(baseline)?;
    Ok(parent)
}

fn verify_moved(fixture: &Fixture, canvas: ScreenRect, baseline: u32) -> Result<()> {
    fixture.check(&fixture.ready.identity)?;
    input_unchanged(baseline)?;
    let control = fixture.control("window")?;
    let left = control.left.checked_add(12).ok_or("移动 fixture x 溢出")?;
    let top = control.top.checked_add(8).ok_or("移动 fixture y 溢出")?;
    let original = &fixture.ready.identity;
    let command = json!({"schema":1,"nonce":fixture.ready.nonce,"sequence":1,"pid":original.pid,
        "creation_filetime":original.creation_filetime,"hwnd":original.hwnd,"thread_id":original.thread_id,
        "token":original.token,"op":"move","left":left,"top":top});
    let directory = fixture.ready_path.parent().ok_or("fixture Ready 无目录")?;
    let command_path = directory.join(&fixture.ready.move_command_filename);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(command_path)?;
    serde_json::to_writer(&mut file, &command)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    let ack_path = directory.join(&fixture.ready.move_ack_filename);
    let start = Instant::now();
    let ack = loop {
        input_unchanged(baseline)?;
        require(start.elapsed() < POLL_LIMIT, "fixture 自有移动命令超时")?;
        match std::fs::read(&ack_path) {
            Ok(bytes) => {
                require(bytes.len() <= 65_536, "fixture ACK 太大")?;
                break serde_json::from_slice::<MoveAck>(&bytes)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => return Err(error.into()),
        }
    };
    require(
        ack.schema == 1
            && ack.nonce == fixture.ready.nonce
            && ack.sequence == 1
            && ack.op == "move"
            && ack.ok
            && ack.before == fixture.ready.identity
            && ack.own_thread_command_acknowledged
            && ack.input_tick_unchanged
            && !ack.product_started,
        "fixture ACK 原身份/nonce/命令不符",
    )?;
    let moved = ack
        .after
        .controls
        .iter()
        .find(|control| control.id == "window")
        .ok_or("移动 ACK 缺少 window")?;
    require(
        moved.left == left && moved.top == top,
        "fixture ACK 未到要求的新物理位置",
    )?;
    fixture.check(&ack.after)?;
    let target = fixture.target(canvas)?;
    require(
        control_selection::probe(canvas, target.clone(), center(fixture.control("button")?)?)
            .is_err(),
        "移动窗口的旧冻结 UIA snapshot 被接受",
    )?;
    require(
        control_selection::text(
            canvas,
            vec![target],
            local_rect(canvas, fixture.control("edit")?)?,
        )
        .is_err(),
        "移动窗口旧冻结文字 snapshot 被接受",
    )?;
    emit(
        "uia_moved_window_old_snapshot_rejected",
        json!({"sequence":1,"original_handles_retained":true,"fixture_stop_requested":false}),
    );
    input_unchanged(baseline)?;
    Ok(())
}

fn install_white(view: &mut CaptureView, target: WindowTarget) -> Result<()> {
    let bounds = view.source.bounds;
    let current = view.source.current;
    let screen_rects: Vec<Rect> = (0..view.source.screen_count())
        .map(|index| view.source.screen_rect(index))
        .collect();
    require(
        u64::from(bounds.width) * u64::from(bounds.height) <= 32_000_000,
        "UIA 专项合成画布太大",
    )?;
    let mut screens = Vec::new();
    for rect in screen_rects {
        let bytes = vec![255; rect.width as usize * rect.height as usize * 4];
        let buffer = image::RgbaImage::from_raw(rect.width, rect.height, bytes)
            .ok_or("UIA 合成屏幕尺寸错误")?;
        screens.push(FrozenScreen {
            rect: ScreenRect {
                left: bounds
                    .left
                    .checked_add(rect.x as i32)
                    .ok_or("合成屏幕 x 溢出")?,
                top: bounds
                    .top
                    .checked_add(rect.y as i32)
                    .ok_or("合成屏幕 y 溢出")?,
                width: rect.width,
                height: rect.height,
            },
            preview: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
        });
    }
    let (source, previews) = CanvasSource::frozen_desktop(FrozenDesktop {
        screens,
        bounds,
        current,
        capture_ms: 0.,
    });
    view.source = source;
    view.previews = previews;
    view.image_canvas = None;
    view.editor = crate::editor::Editor::new(view.source.full(), None);
    view.editor.select_tool(Tool::Crop);
    view.cursor = None;
    view.cursor_preview = None;
    view.include_cursor = false;
    view.magnifier = false;
    view.guides = false;
    view.show_guides = false;
    view.pin_mode = false;
    view.toolbar_panel = ToolbarPanel::None;
    view.window_targets = vec![target];
    view.hover_window = None;
    view.control_selection_ready = true;
    view.auto_adsorb = true;
    view.control_hover = None;
    view.control_request_pending = false;
    view.last_control_request = None;
    view.control_pending_navigation = 0;
    view.control_wheel_remainder = 0.;
    view.pointer_down_start = None;
    view.selection_click_target = None;
    Ok(())
}

struct GuiIdentity {
    input: Input,
    thread_id: u32,
    class: String,
    bounds: ScreenRect,
    baseline: u32,
}
impl GuiIdentity {
    fn bind(input: Input, bounds: ScreenRect, baseline: u32) -> Result<Self> {
        let handle = HWND(input.handle as *mut _);
        let mut pid = 0;
        let mut class = [0_u16; 256];
        let thread_id = unsafe { GetWindowThreadProcessId(handle, Some(&mut pid)) };
        require(
            pid == std::process::id()
                && thread_id != 0
                && thread_id == unsafe { GetCurrentThreadId() },
            "专项 GPUI 原 HWND/PID/TID 不符",
        )?;
        let count = unsafe { GetClassNameW(handle, &mut class) };
        require(
            count > 0 && bounds.width < 65536 && bounds.height < 65536,
            "专项 GPUI class/PresentSize 无效",
        )?;
        Ok(Self {
            input,
            thread_id,
            class: String::from_utf16(&class[..count as usize])?,
            bounds,
            baseline,
        })
    }
    fn serial(&self) -> Result<usize> {
        input_unchanged(self.baseline)?;
        let handle = HWND(self.input.handle as *mut _);
        let mut pid = 0;
        let mut class = [0_u16; 256];
        let mut bounds = RECT::default();
        unsafe {
            require(
                IsWindow(Some(handle)).as_bool()
                    && IsWindowVisible(handle).as_bool()
                    && GetWindowThreadProcessId(handle, Some(&mut pid)) == self.thread_id
                    && pid == std::process::id(),
                "专项 GPUI 原窗口已失效",
            )?;
            let count = GetClassNameW(handle, &mut class);
            require(
                count > 0 && String::from_utf16(&class[..count as usize])? == self.class,
                "专项 GPUI 原 class 已变化",
            )?;
            GetWindowRect(handle, &mut bounds)?;
            require(
                bounds.left == self.bounds.left
                    && bounds.top == self.bounds.top
                    && i64::from(bounds.right) - i64::from(bounds.left)
                        == i64::from(self.bounds.width)
                    && i64::from(bounds.bottom) - i64::from(bounds.top)
                        == i64::from(self.bounds.height),
                "专项 GPUI 不是原全物理画布",
            )?;
            let serial = GetPropW(handle, w!("RustShot.GpuiPresentSerial")).0 as usize;
            if serial > 0 {
                require(
                    GetPropW(handle, w!("RustShot.GpuiPresentSize")).0 as usize
                        == ((self.bounds.width as usize) << 16 | self.bounds.height as usize),
                    "专项 GPUI 全物理 PresentSize 不符",
                )?;
            }
            Ok(serial)
        }
    }
    fn input_target(&self, global: (i32, i32)) -> Result<()> {
        self.serial()?;
        let handle = HWND(self.input.handle as *mut _);
        let point = POINT {
            x: global.0,
            y: global.1,
        };
        unsafe {
            require(
                GetForegroundWindow() == handle
                    && GetActiveWindow() == handle
                    && GetCapture().is_invalid(),
                "专项 GPUI 前台/活动/捕获门失败",
            )?;
            require(
                WindowFromPoint(point) == handle
                    && !MonitorFromPoint(point, MONITOR_DEFAULTTONULL).0.is_null(),
                "专项输入点不在原自有窗口的物理显示器内",
            )?;
        }
        Ok(())
    }
}

async fn pause(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;
}
async fn expect_hover(
    view: &WeakEntity<CaptureView>,
    cx: &mut AsyncApp,
    expected: Rect,
    guard: &GuiIdentity,
    previous_present: usize,
) -> Result<()> {
    let start = Instant::now();
    loop {
        let serial = guard.serial()?;
        if view
            .update(cx, |view, _| {
                view.hover_selection()
                    .is_some_and(|target| target.rect == expected)
                    && !view.control_request_pending
                    && view.control_hover.is_some()
            })
            .map_err(|error| error.to_string())?
            && serial > previous_present
        {
            return Ok(());
        }
        require(
            start.elapsed() < POLL_LIMIT,
            "GPUI 真实悬停/原生滚轮未取得预期 UIA 层级",
        )?;
        pause(cx).await;
    }
}

async fn verify(view: WeakEntity<CaptureView>, input: Input, cx: &mut AsyncApp) -> Result<()> {
    let fixture = cx
        .background_executor()
        .spawn(async { Fixture::bind().map_err(|error| error.to_string()) })
        .await?;
    let baseline = fixture.ready.input_baseline_tick;
    input_unchanged(baseline)?;
    let canvas = view
        .update(cx, |view, _| view.source.bounds)
        .map_err(|error| error.to_string())?;
    let guard = GuiIdentity::bind(input, canvas, baseline)?;
    let start = Instant::now();
    while guard.serial()? == 0 {
        require(
            start.elapsed() < POLL_LIMIT,
            "专项 GPUI 首次全物理 Present 未就绪",
        )?;
        pause(cx).await;
    }
    let (fixture, parent) = cx
        .background_executor()
        .spawn(async move {
            let parent =
                verify_backend(&fixture, canvas, baseline).map_err(|error| error.to_string())?;
            Ok::<_, String>((fixture, parent))
        })
        .await?;
    let target = fixture.target(canvas)?;
    let button = local_rect(canvas, fixture.control("button")?)?;
    let before_white = guard.serial()?;
    view.update(cx, |view, cx| {
        install_white(view, target)?;
        cx.notify();
        Ok::<(), Box<dyn std::error::Error>>(())
    })
    .map_err(|error| error.to_string())??;
    let start = Instant::now();
    while guard.serial()? <= before_white {
        require(start.elapsed() < POLL_LIMIT, "专项合成白底未成功 Present")?;
        pause(cx).await;
    }
    let button_point = (button.x + button.width / 2, button.y + button.height / 2);
    require(
        button_point.0 <= i16::MAX as u32 && button_point.1 <= i16::MAX as u32,
        "UIA smoke mouse message 坐标超出有符号 16 位",
    )?;
    let global_point = center(fixture.control("button")?)?;
    guard.input_target(global_point)?;
    let before_hover = guard.serial()?;
    input.hover(button_point)?;
    expect_hover(&view, cx, button, &guard, before_hover).await?;
    emit(
        "gpui_smoke_move_actual_uia_hover",
        json!({"posted_own_wm_mousemove":true,"system_cursor_moved":false}),
    );
    let logical = (
        button_point.0 as f32 / input.scale,
        button_point.1 as f32 / input.scale,
    );
    guard.input_target(global_point)?;
    let before_wheel = guard.serial()?;
    input.wheel(logical, 120)?;
    expect_hover(&view, cx, parent, &guard, before_wheel).await?;
    emit("gpui_native_wheel_parent", json!({"delta":120}));
    guard.input_target(global_point)?;
    let before_wheel = guard.serial()?;
    input.wheel(logical, -120)?;
    expect_hover(&view, cx, button, &guard, before_wheel).await?;
    emit("gpui_native_wheel_child", json!({"delta":-120}));
    guard.input_target(global_point)?;
    let before_click = guard.serial()?;
    input.click(logical);
    let start = Instant::now();
    loop {
        let serial = guard.serial()?;
        if view
            .update(cx, |view, _| {
                view.editor.crop == Some(button) && !view.editor.is_dragging()
            })
            .map_err(|error| error.to_string())?
            && serial > before_click
        {
            break;
        }
        require(
            start.elapsed() < POLL_LIMIT,
            "GPUI 真实按钮点击没有确认 UIA 控件选区",
        )?;
        pause(cx).await;
    }
    emit(
        "gpui_click_confirms_control_rect",
        json!({"posted_own_button_messages":true,"clipboard_accessed":false}),
    );
    let fixture_identity = report_identity(&fixture.ready.identity);
    let fixture_nonce = fixture.ready.nonce.clone();
    cx.background_executor()
        .spawn(async move {
            verify_moved(&fixture, canvas, baseline).map_err(|error| error.to_string())?;
            let mut fixture = fixture;
            fixture.close_queries().map_err(|error| error.to_string())?;
            Ok::<(), String>(())
        })
        .await?;
    emit(
        "complete",
        json!({"checks":10,"fixture_identity":fixture_identity,"fixture_nonce":fixture_nonce,
        "gpui_hwnd":input.handle.to_string(),"gpui_thread_id":guard.thread_id,"gpui_class":guard.class,
        "input_baseline_tick":baseline,"input_baseline_scope":"fixture Ready captured before product launch; never reset in smoke",
        "synthetic_white_same_screen_topology":true,"uia_queries_on_background_mta":true,
        "fixture_nonce_bound":true,"original_query_handles_retained":true,"fixture_stop_requested":false,
        "clipboard_accessed":false,"png_written":false,"send_input_used":false,"system_cursor_moved":false,
        "real_os_ime_verified":false,"arbitrary_external_app_uia_verified":false}),
    );
    Ok(())
}

pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    *errors.borrow_mut() = Some("控件选区专项尚未完成".into());
    let weak = view.downgrade();
    let input = Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    };
    window.on_next_frame(move |_, cx| {
        cx.spawn(async move |cx| {
            let result: Result<()> = async {
                gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
                let mut own_pid = 0;
                require(unsafe { GetWindowThreadProcessId(HWND(input.handle as *mut _), Some(&mut own_pid)) } != 0
                    && own_pid == std::process::id(), "UIA smoke 输入目标不是产品自有窗口")?;
                verify(weak, input, cx).await
            }.await;
            match result {
                Ok(()) => { *errors.borrow_mut() = None; let _ = cx.update(|cx| cx.quit()); }
                Err(error) => {
                    let message = error.to_string(); let preserve = message == INPUT_CHANGED;
                    *errors.borrow_mut() = Some(message.clone());
                    crate::startup_probe::emit_trace_value(&json!({"case":SUITE,"ok":false,"error":message,"session_preserved":preserve,
                        "fixture_stop_requested":false,"clipboard_accessed":false,"png_written":false}));
                    if !preserve { let _ = cx.update(|cx| cx.quit()); }
                }
            }
        }).detach();
    });
}
