use crate::{BackendCommand, BackendEvent, ListenerDiagnostics, PRIORITY_PROBE_TAG};
use omnitrigger_core::{
    Axis, Button, Decision, Engine, InputEvent, InputKind, MouseButton, Recorder, TriggerSpec,
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    mem::size_of,
    sync::mpsc::{self, Receiver, Sender, SyncSender},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM},
    System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
    UI::{
        Input::KeyboardAndMouse::{
            GetAsyncKeyState, GetKeyNameTextW, MapVirtualKeyW, SendInput, INPUT, INPUT_0,
            INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MAPVK_VK_TO_VSC_EX, MAPVK_VSC_TO_VK_EX,
            MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
            MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
            MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
        },
        WindowsAndMessaging::{
            CallNextHookEx, GetForegroundWindow, GetMessageW, KillTimer, PeekMessageW,
            PostThreadMessageW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT,
            LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
            WH_KEYBOARD_LL, WH_MOUSE_LL, WM_APP, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
            WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEWHEEL,
            WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
        },
    },
};

const COMMAND_MESSAGE: u32 = WM_APP + 0x26;
const REPLAY_TAG: usize = 0x4f4d_4e49;
const EVENT_CAPACITY: usize = 256;

thread_local! {
    // Hook 始终在安装它的消息线程运行；不在 UI 线程共享可变引擎。
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

pub struct Backend {
    commands: Sender<BackendCommand>,
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

impl Backend {
    pub fn start() -> Result<(Self, Receiver<BackendEvent>), String> {
        let (commands_tx, commands_rx) = mpsc::channel();
        let (events_tx, events_rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("omnitrigger-input".into())
            .spawn(move || run_input_thread(commands_rx, events_tx, ready_tx))
            .map_err(|error| format!("无法创建输入线程：{error}"))?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok((
                Self {
                    commands: commands_tx,
                    thread_id,
                    worker: Some(worker),
                },
                events_rx,
            )),
            Ok(Err(error)) => {
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                let _ = worker.join();
                Err("输入线程在初始化完成前结束".into())
            }
        }
    }

    pub fn send(&self, command: BackendCommand) -> Result<(), String> {
        self.commands
            .send(command)
            .map_err(|_| "输入线程已经结束".to_string())?;
        // 先入队再唤醒。消息循环已在 start 返回前创建。
        unsafe { PostThreadMessageW(self.thread_id, COMMAND_MESSAGE, WPARAM(0), LPARAM(0)) }
            .map_err(|error| format!("无法唤醒输入线程：{error}"))
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.send(BackendCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_input_thread(
    commands: Receiver<BackendCommand>,
    events: SyncSender<BackendEvent>,
    ready: SyncSender<Result<u32, String>>,
) {
    let mut message = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
    }
    let module = match unsafe { GetModuleHandleW(None) } {
        Ok(module) => module,
        Err(error) => {
            let _ = ready.send(Err(format!("无法定位应用模块：{error}")));
            return;
        }
    };
    let mut state = ThreadState::new(events.clone());
    state.snapshot_preexisting();
    STATE.with(|slot| *slot.borrow_mut() = Some(state));
    let keyboard = match unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(keyboard_hook),
            Some(HINSTANCE(module.0)),
            0,
        )
    } {
        Ok(hook) => hook,
        Err(error) => {
            STATE.with(|slot| slot.borrow_mut().take());
            let _ = ready.send(Err(format!("无法安装键盘 Hook：{error}")));
            return;
        }
    };
    let mouse = match unsafe {
        SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(HINSTANCE(module.0)), 0)
    } {
        Ok(hook) => hook,
        Err(error) => {
            unsafe {
                let _ = UnhookWindowsHookEx(keyboard);
            }
            STATE.with(|slot| slot.borrow_mut().take());
            let _ = ready.send(Err(format!("无法安装鼠标 Hook：{error}")));
            return;
        }
    };
    let mut shutdown_timer = 0;
    if ready.send(Ok(unsafe { GetCurrentThreadId() })).is_ok() {
        let _ = events.try_send(BackendEvent::Ready);
        let mut shutdown_deadline = None;
        'message_loop: loop {
            let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
            if result <= 0 {
                if result == -1 {
                    let _ =
                        events.try_send(BackendEvent::Error("输入消息循环失败，监听已结束".into()));
                }
                break;
            }
            if message.message == COMMAND_MESSAGE {
                while let Ok(command) = commands.try_recv() {
                    if matches!(command, BackendCommand::Shutdown) {
                        let drained = STATE.with(|slot| {
                            let mut slot = slot.borrow_mut();
                            let state = slot.as_mut().expect("输入状态存在");
                            state.command(BackendCommand::SetEnabled(false));
                            state.owned_press.is_empty()
                        });
                        if drained {
                            break 'message_loop;
                        }
                        // 给用户松开已消费按压的机会；不补发已命中的 CapsLock/鼠标原动作。
                        // 为避免设备移除/长期按住导致关闭卡死，最多等待两秒。
                        shutdown_deadline = Some(Instant::now() + Duration::from_secs(2));
                        shutdown_timer = unsafe { SetTimer(None, 0, 25, None) };
                        if shutdown_timer == 0 {
                            break 'message_loop;
                        }
                        continue;
                    }
                    if shutdown_deadline.is_some() {
                        continue;
                    }
                    STATE.with(|slot| {
                        if let Some(state) = slot.borrow_mut().as_mut() {
                            state.command(command);
                        }
                    });
                }
            }
            if let Some(deadline) = shutdown_deadline {
                let drained = STATE.with(|slot| {
                    slot.borrow()
                        .as_ref()
                        .is_none_or(|state| state.owned_press.is_empty())
                });
                if drained || Instant::now() >= deadline {
                    break;
                }
            }
        }
    }
    unsafe {
        if shutdown_timer != 0 {
            let _ = KillTimer(None, shutdown_timer);
        }
        let _ = UnhookWindowsHookEx(mouse);
        let _ = UnhookWindowsHookEx(keyboard);
    }
    STATE.with(|slot| slot.borrow_mut().take());
}

#[derive(Clone, Copy, Debug)]
struct KeyMetadata {
    scan_code: u16,
    extended: bool,
}

struct RecordingSession {
    recorder: Recorder,
    preexisting: BTreeSet<Button>,
    candidate: Option<TriggerSpec>,
}

struct ThreadState {
    engine: Engine,
    events: SyncSender<BackendEvent>,
    recording: Option<RecordingSession>,
    recording_consumed: BTreeSet<Button>,
    failure_draining: BTreeSet<Button>,
    owned_press: BTreeSet<Button>,
    physical_held: BTreeSet<Button>,
    key_metadata: BTreeMap<Button, KeyMetadata>,
    foreground: usize,
    overflow_reported: bool,
    counters: ListenerDiagnostics,
    last_keyboard: Option<Instant>,
    last_mouse: Option<Instant>,
}

impl ThreadState {
    fn new(events: SyncSender<BackendEvent>) -> Self {
        Self {
            engine: Engine::new(Vec::new()).expect("空绑定列表始终有效"),
            events,
            recording: None,
            recording_consumed: BTreeSet::new(),
            failure_draining: BTreeSet::new(),
            owned_press: BTreeSet::new(),
            physical_held: BTreeSet::new(),
            key_metadata: BTreeMap::new(),
            foreground: unsafe { GetForegroundWindow() }.0 as usize,
            overflow_reported: false,
            counters: ListenerDiagnostics::default(),
            last_keyboard: None,
            last_mouse: None,
        }
    }

    fn snapshot_preexisting(&mut self) {
        // 启动前已有的按压已交付给目标窗口，不能将它们的自动重复误当新前缀。
        // 此处运行在安装 Hook 之前，不能放进 Hook 回调（回调早于系统异步状态更新）。
        for key in 1..=254 {
            if matches!(key, 0x10..=0x12) || unsafe { GetAsyncKeyState(key) } >= 0 {
                continue;
            }
            let button = match key {
                0x01 => Button::Mouse(MouseButton::Left),
                0x02 => Button::Mouse(MouseButton::Right),
                0x04 => Button::Mouse(MouseButton::Middle),
                0x05 => Button::Mouse(MouseButton::X1),
                0x06 => Button::Mouse(MouseButton::X2),
                _ => Button::Key(key as u16),
            };
            self.physical_held.insert(button);
            self.engine.process(InputEvent {
                kind: InputKind::Down(button),
                injected: false,
            });
        }
    }

    fn notify(&mut self, event: BackendEvent) {
        if self.events.try_send(event).is_err() {
            self.overflow_reported = true;
        }
    }

    fn note_callback(&mut self, keyboard: bool, injected: bool, tag: usize) {
        let now = Instant::now();
        if keyboard {
            self.counters.keyboard_events = self.counters.keyboard_events.saturating_add(1);
            self.last_keyboard = Some(now);
        } else {
            self.counters.mouse_events = self.counters.mouse_events.saturating_add(1);
            self.last_mouse = Some(now);
        }
        if injected {
            match tag {
                REPLAY_TAG => {
                    self.counters.self_replayed = self.counters.self_replayed.saturating_add(1)
                }
                PRIORITY_PROBE_TAG => {
                    self.counters.probe_events = self.counters.probe_events.saturating_add(1)
                }
                _ => {
                    self.counters.external_injected =
                        self.counters.external_injected.saturating_add(1);
                }
            }
        }
    }

    fn diagnostics(&self) -> ListenerDiagnostics {
        let mut snapshot = self.counters.clone();
        snapshot.held = self.physical_held.iter().copied().collect();
        snapshot.owned = self.owned_press.iter().copied().collect();
        let age_ms = |time: Instant| time.elapsed().as_millis().min(u64::MAX as u128) as u64;
        snapshot.last_keyboard_ms = self.last_keyboard.map(age_ms);
        snapshot.last_mouse_ms = self.last_mouse.map(age_ms);
        snapshot
    }

    fn command(&mut self, command: BackendCommand) {
        match command {
            BackendCommand::SetBindings(bindings) => {
                if self.recording.is_some() {
                    self.notify(BackendEvent::Error("请先结束录制，再更新触发器".into()));
                } else if let Err(error) = self.engine.replace_bindings(bindings) {
                    self.notify(BackendEvent::Error(error.to_string()));
                }
            }
            BackendCommand::StartRecording => {
                if !self.recording_consumed.is_empty() {
                    self.notify(BackendEvent::Error("请先松开上次录制中按住的按键".into()));
                    return;
                }
                self.engine.cancel();
                self.recording = Some(RecordingSession {
                    recorder: Recorder::new(),
                    preexisting: self.physical_held.clone(),
                    candidate: None,
                });
            }
            BackendCommand::CancelRecording => self.cancel_recording(),
            BackendCommand::SetEnabled(enabled) => {
                self.cancel_recording();
                self.engine.set_enabled(enabled);
            }
            BackendCommand::GetDiagnostics => {
                self.notify(BackendEvent::Diagnostics(self.diagnostics()));
            }
            BackendCommand::Shutdown => {}
        }
    }

    fn cancel_recording(&mut self) {
        if self.recording.take().is_some() {
            self.engine.cancel();
            self.notify(BackendEvent::RecordingCancelled);
        }
    }

    fn input(&mut self, kind: InputKind, metadata: Option<KeyMetadata>) -> HookOutcome {
        let foreground = unsafe { GetForegroundWindow() }.0 as usize;
        if foreground != self.foreground {
            self.counters.foreground_cancellations =
                self.counters.foreground_cancellations.saturating_add(1);
            // 不把旧窗口的缓冲输入补发给新窗口。
            self.engine.cancel();
            self.cancel_recording();
            self.foreground = foreground;
        }
        if self.overflow_reported {
            self.overflow_reported = self
                .events
                .try_send(BackendEvent::Error(
                    "提示队列已满，部分触发通知被丢弃".into(),
                ))
                .is_err();
        }
        match kind {
            InputKind::Down(button) => {
                self.physical_held.insert(button);
                if let Some(metadata) = metadata {
                    self.key_metadata.insert(button, metadata);
                }
            }
            InputKind::Up(button) => {
                self.physical_held.remove(&button);
            }
            InputKind::Wheel { .. } => {}
        }
        let event = InputEvent {
            kind,
            injected: false,
        };
        let failed_button = match kind {
            InputKind::Down(button) | InputKind::Up(button)
                if self.failure_draining.contains(&button) =>
            {
                Some(button)
            }
            _ => None,
        };
        let mut decision = if let Some(button) = failed_button {
            if matches!(kind, InputKind::Up(_)) {
                self.failure_draining.remove(&button);
                self.engine.process(event);
            }
            Decision {
                consume: true,
                ..Decision::default()
            }
        } else if self.recording.is_some() {
            self.record_input(event)
        } else if let InputKind::Down(button) | InputKind::Up(button) = kind {
            if self.recording_consumed.contains(&button) {
                if matches!(kind, InputKind::Up(_)) {
                    self.recording_consumed.remove(&button);
                }
                Decision {
                    consume: true,
                    ..Decision::default()
                }
            } else {
                self.engine.process(event)
            }
        } else {
            self.engine.process(event)
        };
        for id in decision.triggered {
            self.notify(BackendEvent::Triggered(id));
        }
        if !decision.consume && !decision.replay.is_empty() {
            // Hook 回调内不能假定补发已先于原事件交付；把当前事件也放进同一批
            // SendInput 并消费原事件，保证“前缀 → 后继输入”的顺序。
            decision.replay.push(kind);
            decision.consume = true;
        }
        match kind {
            InputKind::Down(button) if decision.consume => {
                self.owned_press.insert(button);
            }
            InputKind::Down(button) | InputKind::Up(button) => {
                self.owned_press.remove(&button);
            }
            InputKind::Wheel { .. } => {}
        }
        let replay = decision
            .replay
            .iter()
            .map(|kind| replay_input(*kind, &self.key_metadata))
            .collect();
        HookOutcome {
            consume: decision.consume,
            replay,
            replay_kinds: decision.replay,
        }
    }

    fn record_input(&mut self, event: InputEvent) -> Decision {
        let session = self.recording.as_mut().expect("录制会话存在");
        if let InputKind::Down(button) | InputKind::Up(button) = event.kind {
            if session.preexisting.contains(&button) {
                if matches!(event.kind, InputKind::Up(_)) {
                    session.preexisting.remove(&button);
                }
                return self.engine.process(event);
            }
        }
        if event.kind == InputKind::Down(Button::Key(0x1b)) {
            self.recording_consumed.insert(Button::Key(0x1b));
            self.cancel_recording();
            return Decision {
                consume: true,
                ..Decision::default()
            };
        }
        let (consume, changed) = match event.kind {
            InputKind::Down(button) => (true, self.recording_consumed.insert(button)),
            InputKind::Up(button) => {
                let removed = self.recording_consumed.remove(&button);
                (removed, removed)
            }
            InputKind::Wheel { .. } => (true, false),
        };
        if !consume {
            return self.engine.process(event);
        }
        if session.candidate.is_none() {
            session.candidate = session.recorder.process(event);
        }
        if self.recording_consumed.is_empty() {
            if let Some(spec) = session.candidate.take() {
                self.recording = None;
                self.notify(BackendEvent::Recorded(spec));
            }
        }
        // 只在按下集合变化时通知界面，键盘 repeat 不产生事件。
        if changed && self.recording.is_some() {
            let held = self.recording_consumed.iter().copied().collect();
            self.notify(BackendEvent::RecordingProgress(held));
        }
        Decision {
            consume: true,
            ..Decision::default()
        }
    }

    fn replay_failed(&mut self, kinds: &[InputKind]) {
        self.counters.replay_failures = self.counters.replay_failures.saturating_add(1);
        self.engine.set_enabled(false);
        // Engine 原本预期回放成功，故标为 Passed。实际未发送/已恢复释放的按压
        // 仍需吞掉物理 repeat/up，不能把没有对应 Down 的 Up 传给目标窗口。
        for kind in kinds {
            match kind {
                InputKind::Down(button) if self.physical_held.contains(button) => {
                    self.failure_draining.insert(*button);
                    self.owned_press.insert(*button);
                }
                _ => {}
            }
        }
    }

    fn replay_succeeded(&mut self, kinds: &[InputKind]) {
        for kind in kinds {
            if let InputKind::Down(button) | InputKind::Up(button) = kind {
                self.owned_press.remove(button);
            }
        }
    }
}

struct HookOutcome {
    consume: bool,
    replay: Vec<INPUT>,
    replay_kinds: Vec<InputKind>,
}

fn process_input(kind: InputKind, metadata: Option<KeyMetadata>) -> bool {
    let outcome = STATE.with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return None;
        };
        slot.as_mut().map(|state| state.input(kind, metadata))
    });
    let Some(outcome) = outcome else {
        return false;
    };
    // 先释放 RefCell 借用，再补发；自身注入在 Hook 入口直接透传。
    if !outcome.replay.is_empty() {
        let sent = unsafe { SendInput(&outcome.replay, size_of::<INPUT>() as i32) } as usize;
        if sent == outcome.replay.len() {
            STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    state.replay_succeeded(&outcome.replay_kinds);
                }
            });
        } else {
            recover_partial_replay(&outcome.replay_kinds[..sent]);
            STATE.with(|slot| {
                if let Some(state) = slot.borrow_mut().as_mut() {
                    state.replay_failed(&outcome.replay_kinds);
                    state.notify(BackendEvent::Error(format!(
                        "输入补发失败（{sent}/{}），监听已暂停；请检查目标应用权限后重新启用",
                        outcome.replay.len()
                    )));
                }
            });
        }
    }
    outcome.consume
}

fn recover_partial_replay(sent: &[InputKind]) {
    let mut held = BTreeSet::new();
    for kind in sent {
        match kind {
            InputKind::Down(button) => {
                held.insert(*button);
            }
            InputKind::Up(button) => {
                held.remove(button);
            }
            InputKind::Wheel { .. } => {}
        }
    }
    if held.is_empty() {
        return;
    }
    let release = STATE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|state| {
                held.into_iter()
                    .map(|button| replay_input(InputKind::Up(button), &state.key_metadata))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    if !release.is_empty() {
        unsafe {
            let _ = SendInput(&release, size_of::<INPUT>() as i32);
        }
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let native = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        note_callback(
            true,
            native.flags.contains(LLKHF_INJECTED),
            native.dwExtraInfo,
        );
        if !native.flags.contains(LLKHF_INJECTED) && native.dwExtraInfo != REPLAY_TAG {
            let down = match wparam.0 as u32 {
                WM_KEYDOWN | WM_SYSKEYDOWN => Some(true),
                WM_KEYUP | WM_SYSKEYUP => Some(false),
                _ => None,
            };
            if let Some(down) = down {
                let button = Button::Key(normalized_key(native));
                let kind = if down {
                    InputKind::Down(button)
                } else {
                    InputKind::Up(button)
                };
                let metadata = KeyMetadata {
                    scan_code: native.scanCode as u16,
                    extended: native.flags.contains(LLKHF_EXTENDED),
                };
                if process_input(kind, Some(metadata)) {
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let native = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        note_callback(
            false,
            native.flags & LLMHF_INJECTED != 0,
            native.dwExtraInfo,
        );
        if native.flags & LLMHF_INJECTED == 0 && native.dwExtraInfo != REPLAY_TAG {
            if let Some(kind) = mouse_input(wparam.0 as u32, native.mouseData) {
                if process_input(kind, None) {
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn note_callback(keyboard: bool, injected: bool, tag: usize) {
    STATE.with(|slot| {
        if let Ok(mut slot) = slot.try_borrow_mut() {
            if let Some(state) = slot.as_mut() {
                state.note_callback(keyboard, injected, tag);
            }
        }
    });
}

fn normalized_key(native: &KBDLLHOOKSTRUCT) -> u16 {
    match native.vkCode {
        0x10 => unsafe { MapVirtualKeyW(native.scanCode, MAPVK_VSC_TO_VK_EX) as u16 },
        0x11 => {
            if native.flags.contains(LLKHF_EXTENDED) {
                0xa3
            } else {
                0xa2
            }
        }
        0x12 => {
            if native.flags.contains(LLKHF_EXTENDED) {
                0xa5
            } else {
                0xa4
            }
        }
        key => key as u16,
    }
}

fn mouse_input(message: u32, data: u32) -> Option<InputKind> {
    let (mouse, down) = match message {
        WM_LBUTTONDOWN => (MouseButton::Left, true),
        WM_LBUTTONUP => (MouseButton::Left, false),
        WM_RBUTTONDOWN => (MouseButton::Right, true),
        WM_RBUTTONUP => (MouseButton::Right, false),
        WM_MBUTTONDOWN => (MouseButton::Middle, true),
        WM_MBUTTONUP => (MouseButton::Middle, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let mouse = match (data >> 16) as u16 {
                1 => MouseButton::X1,
                2 => MouseButton::X2,
                _ => return None,
            };
            (mouse, message == WM_XBUTTONDOWN)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            return Some(InputKind::Wheel {
                axis: if message == WM_MOUSEWHEEL {
                    Axis::Vertical
                } else {
                    Axis::Horizontal
                },
                delta: (data >> 16) as i16 as i32,
            })
        }
        _ => return None,
    };
    let button = Button::Mouse(mouse);
    Some(if down {
        InputKind::Down(button)
    } else {
        InputKind::Up(button)
    })
}

fn replay_input(kind: InputKind, metadata: &BTreeMap<Button, KeyMetadata>) -> INPUT {
    match kind {
        InputKind::Down(Button::Key(key)) | InputKind::Up(Button::Key(key)) => {
            let fallback_scan = unsafe { MapVirtualKeyW(key.into(), MAPVK_VK_TO_VSC_EX) };
            let metadata = metadata
                .get(&Button::Key(key))
                .copied()
                .unwrap_or(KeyMetadata {
                    scan_code: (fallback_scan & 0xff) as u16,
                    extended: fallback_scan & 0xff00 == 0xe000,
                });
            let mut flags = KEYBD_EVENT_FLAGS(0);
            if metadata.scan_code != 0 {
                flags |= KEYEVENTF_SCANCODE;
            }
            if metadata.extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if matches!(kind, InputKind::Up(_)) {
                flags |= KEYEVENTF_KEYUP;
            }
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(if metadata.scan_code == 0 { key } else { 0 }),
                        wScan: metadata.scan_code,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: REPLAY_TAG,
                    },
                },
            }
        }
        InputKind::Down(Button::Mouse(mouse)) | InputKind::Up(Button::Mouse(mouse)) => {
            let down = matches!(kind, InputKind::Down(_));
            let (flags, data) = match (mouse, down) {
                (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                (MouseButton::X1, true) => (MOUSEEVENTF_XDOWN, 1),
                (MouseButton::X1, false) => (MOUSEEVENTF_XUP, 1),
                (MouseButton::X2, true) => (MOUSEEVENTF_XDOWN, 2),
                (MouseButton::X2, false) => (MOUSEEVENTF_XUP, 2),
            };
            INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        mouseData: data,
                        dwFlags: flags,
                        dwExtraInfo: REPLAY_TAG,
                        ..MOUSEINPUT::default()
                    },
                },
            }
        }
        InputKind::Wheel { axis, delta } => INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    mouseData: delta as u32,
                    dwFlags: if axis == Axis::Vertical {
                        MOUSEEVENTF_WHEEL
                    } else {
                        MOUSEEVENTF_HWHEEL
                    },
                    dwExtraInfo: REPLAY_TAG,
                    ..MOUSEINPUT::default()
                },
            },
        },
    }
}

pub(crate) fn local_clock() -> (u16, u16, u16) {
    let time = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    (time.wHour, time.wMinute, time.wSecond)
}

pub(crate) fn key_name(key: u16) -> Option<String> {
    let scan = unsafe { MapVirtualKeyW(key.into(), MAPVK_VK_TO_VSC_EX) };
    let mut lparam = ((scan & 0xff) << 16) as i32;
    if scan & 0xff00 == 0xe000 {
        lparam |= 1 << 24;
    }
    let mut buffer = [0u16; 64];
    let count = unsafe { GetKeyNameTextW(lparam, &mut buffer) };
    (count > 0).then(|| String::from_utf16_lossy(&buffer[..count as usize]))
}

pub(crate) fn show_notification_window(
    hwnd: isize,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<(), String> {
    use windows::Win32::{
        Foundation::{GetLastError, SetLastError, HWND, WIN32_ERROR},
        UI::WindowsAndMessaging::{
            GetWindowLongPtrW, IsWindow, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE,
            HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_SHOWWINDOW, WS_CAPTION,
            WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_THICKFRAME,
        },
    };
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    if width <= 0 || height <= 0 || !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return Err("提示窗口句柄或尺寸无效".into());
    }
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            style | WS_EX_NOACTIVATE.0 as isize | WS_EX_TOOLWINDOW.0 as isize,
        );
        let error = GetLastError();
        if previous == 0 && error.0 != 0 {
            return Err(format!("无法设置提示窗口样式：{error:?}"));
        }
        // 去掉窗框，使客户区与给定尺寸一致；否则界面按窗口尺寸布局，右下角会被裁掉。
        let frame = GetWindowLongPtrW(hwnd, GWL_STYLE);
        SetLastError(WIN32_ERROR(0));
        let previous = SetWindowLongPtrW(
            hwnd,
            GWL_STYLE,
            frame & !(WS_CAPTION.0 as isize | WS_THICKFRAME.0 as isize),
        );
        let error = GetLastError();
        if previous == 0 && error.0 != 0 {
            return Err(format!("无法设置提示窗口边框：{error:?}"));
        }
        SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x,
            y,
            width,
            height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_FRAMECHANGED,
        )
        .map_err(|error| format!("无法显示提示窗口：{error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mouse_mapping_preserves_signed_high_precision_delta_and_axis() {
        assert_eq!(
            mouse_input(WM_MOUSEWHEEL, (-30i16 as u16 as u32) << 16),
            Some(InputKind::Wheel {
                axis: Axis::Vertical,
                delta: -30
            })
        );
        assert_eq!(
            mouse_input(WM_MOUSEHWHEEL, 120 << 16),
            Some(InputKind::Wheel {
                axis: Axis::Horizontal,
                delta: 120
            })
        );
        assert_eq!(
            mouse_input(WM_XBUTTONDOWN, 2 << 16),
            Some(InputKind::Down(Button::Mouse(MouseButton::X2)))
        );
        assert_eq!(mouse_input(WM_XBUTTONUP, 3 << 16), None);
    }
    #[test]
    fn right_control_and_alt_are_distinct_from_left() {
        let mut native = KBDLLHOOKSTRUCT {
            vkCode: 0x11,
            ..KBDLLHOOKSTRUCT::default()
        };
        assert_eq!(normalized_key(&native), 0xa2);
        native.flags = LLKHF_EXTENDED;
        assert_eq!(normalized_key(&native), 0xa3);
        native.vkCode = 0x12;
        assert_eq!(normalized_key(&native), 0xa5);
    }
    #[test]
    fn replay_uses_original_extended_scan_code_and_injection_tag() {
        let mut metadata = BTreeMap::new();
        metadata.insert(
            Button::Key(0xa3),
            KeyMetadata {
                scan_code: 0x1d,
                extended: true,
            },
        );
        let input = replay_input(InputKind::Up(Button::Key(0xa3)), &metadata);
        let keyboard = unsafe { input.Anonymous.ki };
        assert_eq!(keyboard.wScan, 0x1d);
        assert_eq!(keyboard.wVk.0, 0);
        assert!(keyboard
            .dwFlags
            .contains(KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP));
        assert_eq!(keyboard.dwExtraInfo, REPLAY_TAG);
    }
    #[test]
    fn recording_ignores_preexisting_mouse_release_and_consumes_complete_chord() {
        let (tx, rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let mut state = ThreadState::new(tx);
        let mouse = Button::Mouse(MouseButton::Left);
        state.physical_held.insert(mouse);
        state.command(BackendCommand::StartRecording);
        assert!(
            !state
                .record_input(InputEvent {
                    kind: InputKind::Up(mouse),
                    injected: false
                })
                .consume
        );
        let caps = Button::Key(0x14);
        let w = Button::Key(0x57);
        for kind in [
            InputKind::Down(caps),
            InputKind::Down(caps),
            InputKind::Down(w),
            InputKind::Up(w),
            InputKind::Up(caps),
        ] {
            assert!(
                state
                    .record_input(InputEvent {
                        kind,
                        injected: false
                    })
                    .consume
            );
        }
        let events: Vec<_> = rx.try_iter().collect();
        let progress: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                BackendEvent::RecordingProgress(held) => Some(held.clone()),
                _ => None,
            })
            .collect();
        // repeat 不重复通知；完成录制的最后一次释放直接发送 Recorded。
        assert_eq!(progress, vec![vec![caps], vec![caps, w], vec![caps]]);
        assert!(
            matches!(events.last(), Some(BackendEvent::Recorded(spec)) if spec.held == vec![caps] && spec.activation == omnitrigger_core::Activation::Press(w))
        );
        assert!(state.recording.is_none());
    }
    #[test]
    fn escape_cancel_keeps_release_consumed() {
        let (tx, rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let mut state = ThreadState::new(tx);
        state.command(BackendCommand::StartRecording);
        assert!(
            state
                .record_input(InputEvent {
                    kind: InputKind::Down(Button::Key(0x1b)),
                    injected: false
                })
                .consume
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(BackendEvent::RecordingCancelled)
        ));
        assert!(state.recording.is_none());
        assert!(state.recording_consumed.contains(&Button::Key(0x1b)));
    }

    #[test]
    fn fallback_batches_current_down_and_failure_drains_its_release() {
        let (tx, _) = mpsc::sync_channel(EVENT_CAPACITY);
        let mut state = ThreadState::new(tx);
        let caps = Button::Key(0x14);
        let c = Button::Key(0x43);
        state
            .engine
            .replace_bindings(vec![TriggerSpec {
                id: 1,
                held: vec![caps],
                activation: omnitrigger_core::Activation::Press(Button::Key(0x57)),
            }])
            .unwrap();
        assert!(state.input(InputKind::Down(caps), None).consume);
        let fallback = state.input(InputKind::Down(c), None);
        assert!(fallback.consume);
        assert_eq!(
            fallback.replay_kinds,
            vec![InputKind::Down(caps), InputKind::Down(c)]
        );
        state.replay_failed(&fallback.replay_kinds);
        assert!(state.failure_draining.contains(&caps));
        assert!(state.failure_draining.contains(&c));
        assert!(state.input(InputKind::Down(c), None).consume);
        assert!(state.input(InputKind::Up(caps), None).consume);
        assert!(state.input(InputKind::Up(c), None).consume);
        assert!(state.owned_press.is_empty());
        state.engine.set_enabled(true);
        assert!(!state.input(InputKind::Down(c), None).consume);
    }

    #[test]
    fn successful_fallback_hands_held_press_back_to_physical_release() {
        let (tx, _) = mpsc::sync_channel(EVENT_CAPACITY);
        let mut state = ThreadState::new(tx);
        let caps = Button::Key(0x14);
        state
            .engine
            .replace_bindings(vec![TriggerSpec {
                id: 1,
                held: vec![caps],
                activation: omnitrigger_core::Activation::Press(Button::Key(0x57)),
            }])
            .unwrap();
        state.input(InputKind::Down(caps), None);
        assert!(state.owned_press.contains(&caps));
        let fallback = state.input(InputKind::Down(Button::Key(0x43)), None);
        state.replay_succeeded(&fallback.replay_kinds);
        assert!(state.owned_press.is_empty());
        assert!(!state.input(InputKind::Up(caps), None).consume);
    }

    #[test]
    fn wheel_recording_waits_for_side_button_release() {
        let (tx, rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let mut state = ThreadState::new(tx);
        state.command(BackendCommand::StartRecording);
        let side = Button::Mouse(MouseButton::X1);
        assert!(
            state
                .record_input(InputEvent {
                    kind: InputKind::Down(side),
                    injected: false
                })
                .consume
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(BackendEvent::RecordingProgress(held)) if held == vec![side]
        ));
        assert!(
            state
                .record_input(InputEvent {
                    kind: InputKind::Wheel {
                        axis: Axis::Vertical,
                        delta: 30
                    },
                    injected: false
                })
                .consume
        );
        assert!(rx.try_recv().is_err());
        assert!(
            state
                .record_input(InputEvent {
                    kind: InputKind::Up(side),
                    injected: false
                })
                .consume
        );
        assert!(
            matches!(rx.try_recv(), Ok(BackendEvent::Recorded(spec)) if spec.held == vec![side] && spec.activation == omnitrigger_core::Activation::Wheel(omnitrigger_core::WheelDirection::Up))
        );
    }

    #[test]
    fn startup_held_press_remains_passed_during_recording() {
        let (tx, _) = mpsc::sync_channel(EVENT_CAPACITY);
        let mut state = ThreadState::new(tx);
        let caps = Button::Key(0x14);
        state.physical_held.insert(caps);
        state.engine.process(InputEvent {
            kind: InputKind::Down(caps),
            injected: false,
        });
        state.command(BackendCommand::StartRecording);
        assert!(
            !state
                .record_input(InputEvent {
                    kind: InputKind::Down(caps),
                    injected: false
                })
                .consume
        );
        assert!(
            !state
                .record_input(InputEvent {
                    kind: InputKind::Up(caps),
                    injected: false
                })
                .consume
        );
        assert!(state.recording_consumed.is_empty());
    }

    #[test]
    fn callback_diagnostics_classify_sources_without_changing_match_state() {
        let (tx, _) = mpsc::sync_channel(EVENT_CAPACITY);
        let mut state = ThreadState::new(tx);
        let caps = Button::Key(0x14);
        let w = Button::Key(0x57);
        state
            .engine
            .replace_bindings(vec![TriggerSpec {
                id: 9,
                held: vec![caps],
                activation: omnitrigger_core::Activation::Press(w),
            }])
            .unwrap();
        assert!(
            state
                .engine
                .process(InputEvent {
                    kind: InputKind::Down(caps),
                    injected: false
                })
                .consume
        );
        assert!(state.diagnostics().last_keyboard_ms.is_none());
        assert!(state.diagnostics().last_mouse_ms.is_none());

        state.note_callback(true, false, PRIORITY_PROBE_TAG);
        state.note_callback(true, true, REPLAY_TAG);
        state.note_callback(true, true, 0x12345);
        state.note_callback(false, true, PRIORITY_PROBE_TAG);
        state.note_callback(false, false, 0);

        let snapshot = state.diagnostics();
        assert_eq!(snapshot.keyboard_events, 3);
        assert_eq!(snapshot.mouse_events, 2);
        assert_eq!(snapshot.external_injected, 1);
        assert_eq!(snapshot.self_replayed, 1);
        assert_eq!(snapshot.probe_events, 1);
        assert!(snapshot.last_keyboard_ms.is_some());
        assert!(snapshot.last_mouse_ms.is_some());
        assert_eq!(
            state
                .engine
                .process(InputEvent {
                    kind: InputKind::Down(w),
                    injected: false
                })
                .triggered,
            vec![9]
        );
    }

    #[test]
    fn diagnostics_snapshot_copies_held_and_owned_without_resetting_ledger() {
        let (tx, rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let mut state = ThreadState::new(tx);
        let caps = Button::Key(0x14);
        let side = Button::Mouse(MouseButton::X1);
        state.physical_held.extend([caps, side]);
        state.owned_press.insert(caps);
        state.command(BackendCommand::GetDiagnostics);
        let Ok(BackendEvent::Diagnostics(mut snapshot)) = rx.try_recv() else {
            panic!("应返回诊断快照");
        };
        assert_eq!(snapshot.held, vec![caps, side]);
        assert_eq!(snapshot.owned, vec![caps]);
        snapshot.held.clear();
        snapshot.owned.clear();
        let unchanged = state.diagnostics();
        assert_eq!(unchanged.held, vec![caps, side]);
        assert_eq!(unchanged.owned, vec![caps]);
    }
}
