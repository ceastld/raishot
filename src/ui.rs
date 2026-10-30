use crate::{capture::memory_mib, hotkey::HotkeyTarget, tray, Result};
#[path = "resident_pin_restore.rs"]
mod resident_pin_restore;
use std::{
    mem::size_of,
    os::windows::process::CommandExt,
    process::{Child, Command},
    time::{Duration, Instant},
};
use windows::{
    core::w,
    Win32::{
        Foundation::*,
        System::{
            LibraryLoader::GetModuleHandleW,
            Threading::{
                CreateMutexW, GetCurrentThreadId, OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS,
            },
        },
        UI::{HiDpi::*, Input::Ime::ImmDisableIME, Shell::*, WindowsAndMessaging::*},
    },
};
pub const RELOAD_MESSAGE: u32 = WM_APP + 9;
pub(crate) const CAPTURE_STARTED_MESSAGE: u32 = WM_APP + 11;
const HOTKEY_RECORD_MESSAGE: u32 = WM_APP + 13;
const HOTKEY_RECORD_TIMER: usize = 12;
/// 请托盘开始截图；主界面“新建”菜单也从这里发起。
pub(crate) const CAPTURE_MESSAGE: u32 = WM_APP + 3;
pub(crate) const PIN_MESSAGE: u32 = WM_APP + 10;
const EXIT_MESSAGE: u32 = WM_APP + 5;
const STANDBY_TIMER: usize = 14;
#[cfg(test)]
const HOTKEY: i32 = 1;

pub(crate) fn read_hotkey_status(
    requested: &crate::preferences::Preferences,
    requested_after: u64,
) -> Option<(bool, String)> {
    crate::hotkey_status::read(requested, requested_after)
}
pub(crate) fn set_hotkey_recording(recording: bool, owner_window: usize) {
    if std::env::var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY").as_deref() == Ok("1") {
        return;
    }
    unsafe {
        if let Ok(tray) = FindWindowW(w!("RustShot.MVP.Window"), None) {
            let token = if recording {
                owner_window as isize
            } else {
                -(owner_window as isize)
            };
            let _ = PostMessageW(
                Some(tray),
                HOTKEY_RECORD_MESSAGE,
                WPARAM(std::process::id() as usize),
                LPARAM(token),
            );
        }
    }
}
/// 捕获互斥体建立后通知托盘；无托盘的 CLI 会话无需通知。
pub(crate) fn report_capture_started() {
    unsafe {
        if let Ok(resident) = FindWindowW(w!("RustShot.MVP.Window"), None) {
            if !resident.0.is_null() {
                let _ = PostMessageW(
                    Some(resident),
                    CAPTURE_STARTED_MESSAGE,
                    WPARAM(std::process::id() as usize),
                    LPARAM(0),
                );
            }
        }
    }
}
fn starts_capture(argument: &str) -> bool {
    matches!(
        argument,
        "--region" | "--fullscreen" | "--all-screens" | "--smoke"
    )
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WorkerLaunch {
    Wait,
    Spawn,
    DetachAndSpawn,
}
struct CaptureDispatchTrace {
    begin: i64,
    end: i64,
    request_qpc: i64,
    source_message: u32,
    source_window: usize,
    previous: Option<MessageDispatchTrace>,
}
#[derive(Clone, Copy)]
struct MessageDispatchTrace {
    message: u32,
    window: usize,
    get_begin: i64,
    get_end: i64,
    dispatch_begin: i64,
    dispatch_end: i64,
}
// 仅 Trace 创建；作用域退出包括 continue/break/?，不会漏掉提前结束的外层处理。
// 此记录由消息循环独占，WindowProc 不访问它。
struct MessageDispatchGuard<'a> {
    previous: &'a mut Option<MessageDispatchTrace>,
    current: MessageDispatchTrace,
}
impl Drop for MessageDispatchGuard<'_> {
    fn drop(&mut self) {
        self.current.dispatch_end = crate::capture_standby::qpc();
        *self.previous = Some(self.current);
    }
}
fn worker_launch(running: bool, pending_capture: bool, capture_busy: bool) -> WorkerLaunch {
    if capture_busy || running && pending_capture {
        WorkerLaunch::Wait
    } else if running {
        WorkerLaunch::DetachAndSpawn
    } else {
        WorkerLaunch::Spawn
    }
}
const STANDBY_QUEUE_TIMEOUT: Duration = Duration::from_secs(10);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingCapture {
    mode: usize,
    quick: u32,
    qpc: i64,
    queued_at: Instant,
}
impl PendingCapture {
    fn new(mode: usize, qpc: i64, queued_at: Instant) -> Self {
        Self {
            mode,
            quick: 0,
            qpc,
            queued_at,
        }
    }
}
fn expire_standby_pending(
    pending: &mut Option<PendingCapture>,
    now: Instant,
    unpublished: bool,
) -> bool {
    if unpublished
        && pending.is_some_and(|request| {
            now.saturating_duration_since(request.queued_at) >= STANDBY_QUEUE_TIMEOUT
        })
    {
        pending.take();
        true
    } else {
        false
    }
}
// READY 与健康检查均在同一外层消息循环消费；取出后由原 dispatch 恢复未就绪请求。
// 没有排队请求时仅绑定待命进程，绝不因计时器自动截图。
fn take_ready_pending(ready: bool, pending: &mut Option<PendingCapture>) -> Option<PendingCapture> {
    if ready {
        pending.take()
    } else {
        None
    }
}
fn retry_after_submission_error(
    request: PendingCapture,
    request_published: bool,
) -> Option<PendingCapture> {
    (!request_published).then_some(request)
}
fn worker_argument(message: u32, parameter: usize, smoke: bool) -> &'static str {
    let mode = if message == WM_HOTKEY {
        0
    } else {
        parameter & 0xf
    };
    if mode & 8 != 0 {
        "--settings"
    } else if mode & 4 != 0 {
        "--history"
    } else if smoke {
        "--smoke"
    } else if mode & 2 != 0 {
        "--all-screens"
    } else if mode & 1 != 0 {
        "--fullscreen"
    } else {
        "--region"
    }
}
// 同步 Shell 回调只投递事件，状态留在外层消息循环，避免重入可变引用。
unsafe extern "system" fn window_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if message == WM_COPYDATA && lp.0 != 0 {
        let data =
            unsafe { &*(lp.0 as *const windows::Win32::System::DataExchange::COPYDATASTRUCT) };
        let accepted = unsafe {
            crate::tray_request::accept(hwnd, data) || crate::quick_toast::accept(hwnd, data)
        };
        return LRESULT(accepted as isize);
    }
    if message >= 0xC000 && message == tray::taskbar_created() {
        let _ = PostMessageW(Some(hwnd), tray::RESTORE, WPARAM(0), LPARAM(0));
        return LRESULT(0);
    }
    match message {
        tray::NOTIFICATION => {
            let _ = PostMessageW(Some(hwnd), tray::EVENT, wp, lp);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = PostMessageW(Some(hwnd), EXIT_MESSAGE, WPARAM(0), LPARAM(0));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wp, lp),
    }
}
struct RegisteredQuickAction {
    id: String,
    recipe: u32,
    hotkey: crate::hotkey_registration::HotkeyRegistration,
}
fn quick_native_binding(
    spec: crate::hotkey::HotkeySpec,
) -> (
    windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS,
    u32,
) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let mut modifiers = MOD_NOREPEAT;
    for (bit, native) in [
        (crate::hotkey::CTRL, MOD_CONTROL),
        (crate::hotkey::ALT, MOD_ALT),
        (crate::hotkey::SHIFT, MOD_SHIFT),
        (crate::hotkey::WIN, MOD_WIN),
    ] {
        if spec.modifiers & bit != 0 {
            modifiers |= native;
        }
    }
    (modifiers, spec.key)
}
fn quick_hotkey_spec(
    binding: Option<(
        windows::Win32::UI::Input::KeyboardAndMouse::HOT_KEY_MODIFIERS,
        u32,
    )>,
) -> Option<crate::hotkey::HotkeySpec> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let (native, key) = binding?;
    let mut modifiers = 0;
    for (bit, flag) in [
        (crate::hotkey::CTRL, MOD_CONTROL),
        (crate::hotkey::ALT, MOD_ALT),
        (crate::hotkey::SHIFT, MOD_SHIFT),
        (crate::hotkey::WIN, MOD_WIN),
    ] {
        if native.contains(flag) {
            modifiers |= bit;
        }
    }
    Some(crate::hotkey::HotkeySpec { modifiers, key })
}

fn menu_command(id: u32) -> Option<HotkeyTarget> {
    Some(match id {
        tray::REGION => HotkeyTarget::Capture,
        tray::FULL_SCREEN => HotkeyTarget::CurrentScreen,
        tray::ALL_SCREENS => HotkeyTarget::AllScreens,
        tray::PIN_CLIPBOARD => HotkeyTarget::ClipboardPin,
        tray::PIN_FILE => HotkeyTarget::PinFile,
        tray::PIN_RESTORE => HotkeyTarget::PinRestore,
        tray::PIN_WORKSPACE => HotkeyTarget::PinWorkspace,
        tray::PIN_INTERACT => HotkeyTarget::PinInteract,
        tray::PIN_TOGGLE_GROUP => HotkeyTarget::PinToggleGroup,
        tray::PIN_NEXT_GROUP => HotkeyTarget::PinNextGroup,
        tray::PIN_PREVIOUS_GROUP => HotkeyTarget::PinPreviousGroup,
        tray::HISTORY => HotkeyTarget::History,
        tray::SETTINGS => HotkeyTarget::Settings,
        _ => return None,
    })
}

fn command_needs_foreground(target: HotkeyTarget) -> bool {
    !matches!(
        target,
        HotkeyTarget::PinToggleGroup
            | HotkeyTarget::PinInteract
            | HotkeyTarget::PinNextGroup
            | HotkeyTarget::PinPreviousGroup
    )
}

fn capture_command_parameter(target: HotkeyTarget) -> Option<usize> {
    match target {
        HotkeyTarget::Capture => Some(0),
        HotkeyTarget::CurrentScreen => Some(1),
        HotkeyTarget::AllScreens => Some(2),
        HotkeyTarget::History => Some(4),
        HotkeyTarget::Settings => Some(8),
        _ => None,
    }
}

/// 启动时尚无旧注册可回退，按目录顺序保留首个有效绑定，冲突项只在本次运行停用。
fn startup_hotkey_preferences(
    requested: &crate::preferences::Preferences,
) -> (
    crate::preferences::Preferences,
    Vec<(HotkeyTarget, HotkeyTarget)>,
) {
    let mut active = requested.clone();
    let mut accepted = Vec::new();
    let mut conflicts = Vec::new();
    for target in HotkeyTarget::ALL {
        let Some(trigger) = requested.trigger_for(target) else {
            continue;
        };
        if let Some((earlier, _)) = accepted
            .iter()
            .find(|(_, previous)| crate::trigger::triggers_conflict(previous, &trigger))
        {
            conflicts.push((*earlier, target));
            active.set_hotkey_mode(target, 4);
        } else {
            accepted.push((target, trigger));
        }
    }
    (active, conflicts)
}

/// 某项注册失败回到旧绑定后，重新检查“实际保留值 + 尚待应用值”的组合。
/// 先撤回引入冲突的变更；若回退基线本身不可共存，则停用冲突的监听项。
fn rollback_conflicting_changes(
    active: &mut crate::preferences::Preferences,
    fallback: &crate::preferences::Preferences,
) -> Vec<HotkeyTarget> {
    let mut restored = Vec::new();
    while let Some((first, second)) = active.hotkey_conflict_pair() {
        let changed: Vec<_> = [first, second]
            .into_iter()
            .filter(|target| active.trigger_for(*target) != fallback.trigger_for(*target))
            .collect();
        if changed.is_empty() {
            let target = if active.input_trigger_for(first).is_some() {
                first
            } else {
                second
            };
            active.set_hotkey_mode(target, 4);
            if !restored.contains(&target) {
                restored.push(target);
            }
        } else {
            for target in changed {
                active.restore_hotkey_from(target, fallback);
                if !restored.contains(&target) {
                    restored.push(target);
                }
            }
        }
    }
    restored
}

struct NativeState {
    quick_actions: crate::quick_action_store::QuickActionStore,
    quick_hotkeys: Vec<RegisteredQuickAction>,
    hwnd: HWND,
    mutex: HANDLE,
    tray: NOTIFYICONDATAW,
    command_hotkeys: Vec<(HotkeyTarget, crate::hotkey_registration::HotkeyRegistration)>,
    /// 模式 6 的键盘／鼠标组合；没有组合时不安装输入 Hook。
    input_triggers: crate::trigger_listener::TriggerListener,
    hotkey_preferences: crate::preferences::Preferences,
    hotkey_recording: Option<(u32, HWND)>,
    pending_hotkey_reload: bool,
    last_empty_pin_notice: Option<Instant>,
    modern_tray: bool,
    worker: Option<Child>,
    worker_pending_capture: bool,
    worker_is_capture: bool,
    pin_workers: Vec<Child>,
    startup_pin_restore: resident_pin_restore::Check,
    standby: Option<crate::capture_standby::Slot>,
    standby_pending: Option<PendingCapture>,
    standby_enabled: bool,
    standby_submission_trace: Option<crate::capture_standby::SubmissionTrace>,
}

impl NativeState {
    fn resolve_hotkey_conflicts(
        &mut self,
        active: &mut crate::preferences::Preferences,
        previous: &crate::preferences::Preferences,
        failed: &mut Vec<HotkeyTarget>,
        errors: &mut Vec<String>,
    ) {
        let mut fallback = previous.clone();
        loop {
            let targets = rollback_conflicting_changes(active, &fallback);
            if targets.is_empty() {
                break;
            }
            for target in targets {
                if !failed.contains(&target) {
                    failed.push(target);
                    errors.push(trf!(
                        "{} 与保留的原快捷键冲突，已撤回此次变更。",
                        "{} conflicts with a retained binding; this change was reverted.",
                        target.label()
                    ));
                }
                if let Err(error) = self
                    .hotkey_registration(target)
                    .set(active.hotkey_for(target))
                {
                    errors.push(trf!(
                        "恢复 {} 失败：{}",
                        "Failed to restore {}: {}",
                        target.label(),
                        error
                    ));
                    if let Some(spec) =
                        quick_hotkey_spec(self.hotkey_registration(target).binding())
                    {
                        *active = active.with_hotkey_change(target, 3, Some(spec));
                    } else {
                        active.set_hotkey_mode(target, 4);
                    }
                    // 本次无法取回的旧键不反复重试；其它冲突项向当前实际注册让位。
                    fallback.restore_hotkey_from(target, active);
                }
            }
        }
    }

    fn pin_owners(&mut self) -> Vec<u32> {
        self.pin_workers
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
        let mut owners: Vec<_> = self.pin_workers.iter().map(Child::id).collect();
        if let Some(child) = self.worker.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                owners.push(child.id());
            }
        }
        owners
    }

    fn empty_pin_notice(&mut self) {
        let now = Instant::now();
        if self
            .last_empty_pin_notice
            .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(3))
        {
            self.last_empty_pin_notice = Some(now);
            balloon(
                self,
                tr!(
                    "当前分组没有打开的贴图。",
                    "There are no open pins in this group."
                ),
                false,
            );
        }
    }

    /// 菜单与全局快捷键共享命令分派；分组操作不创建 GPU 会话。
    fn dispatch_command(&mut self, target: HotkeyTarget) -> Result<()> {
        if let Some(parameter) = capture_command_parameter(target) {
            unsafe {
                PostMessageW(
                    Some(self.hwnd),
                    CAPTURE_MESSAGE,
                    WPARAM(parameter),
                    LPARAM(crate::capture_standby::qpc() as isize),
                )?;
            }
            return Ok(());
        }
        match target {
            HotkeyTarget::ClipboardPin => self.spawn_pin("--pin-clipboard")?,
            HotkeyTarget::PinFile => self.spawn_pin("--pin-file")?,
            HotkeyTarget::PinRestore => self.spawn_pin("--pin-restore")?,
            HotkeyTarget::PinWorkspace => self.spawn_pin("--pin-workspace")?,
            HotkeyTarget::PinToggleGroup => {
                let owners = self.pin_owners();
                if crate::pin_workspace_runtime::toggle_current_group(&owners)? == 0 {
                    self.empty_pin_notice();
                }
            }
            HotkeyTarget::PinNextGroup | HotkeyTarget::PinPreviousGroup => {
                let name = crate::pin_workspace_runtime::cycle_group(
                    target == HotkeyTarget::PinNextGroup,
                )?;
                balloon(
                    self,
                    &trf!("当前分组：{}", "Current group: {}", name),
                    false,
                );
            }
            HotkeyTarget::PinInteract => {
                let owners = self.pin_owners();
                let count = crate::pin_workspace_runtime::manage(
                    &owners,
                    crate::pin_native::Manage::Interact,
                )?;
                if count + crate::pin_native::manage(&owners, crate::pin_native::Manage::Interact)
                    == 0
                {
                    self.empty_pin_notice();
                }
            }
            _ => unreachable!("截图类命令已在上方分派"),
        }
        Ok(())
    }

    fn reload_quick_actions(&mut self) {
        let requested = crate::quick_action_store::QuickActionStore::load();
        let mut active = requested.clone();
        let desired: Vec<_> = requested
            .actions
            .iter()
            .filter(|a| a.enabled && a.hotkey.is_some())
            .take(crate::quick_action::MAX_ACTION_HOTKEYS)
            .cloned()
            .collect();
        let mut rollbacks = Vec::new();
        let mut errors = Vec::new();
        self.quick_hotkeys.retain(|old| {
            requested
                .find(&old.id)
                .is_some_and(|a| a.enabled && a.hotkey.is_some())
        });
        for action in desired {
            let spec = action.hotkey.unwrap();
            let binding = Some(quick_native_binding(spec));
            let conflicts = HotkeyTarget::ALL.iter().any(|target| {
                self.hotkey_preferences
                    .trigger_for(*target)
                    .is_some_and(|existing| {
                        crate::trigger::triggers_conflict(
                            &existing,
                            &crate::trigger::Trigger::Hotkey(spec),
                        )
                    })
            });
            let Ok(recipe) = action.recipe.pack() else {
                continue;
            };
            let old = self
                .quick_hotkeys
                .iter_mut()
                .find(|old| old.id == action.id);
            let (failure, actual) = if let Some(old) = old {
                // 配方独立于绑定；保留旧热键时也执行用户已保存的新步骤。
                old.recipe = recipe;
                if conflicts {
                    (
                        Some(tr!("与现有热键冲突", "Conflicts with an existing hotkey").to_owned()),
                        quick_hotkey_spec(old.hotkey.binding()),
                    )
                } else {
                    match old.hotkey.set(binding) {
                        Ok(_) => (None, Some(spec)),
                        Err(error) => (
                            Some(error.to_string()),
                            quick_hotkey_spec(old.hotkey.binding()),
                        ),
                    }
                }
            } else if conflicts {
                (
                    Some(tr!("与现有热键冲突", "Conflicts with an existing hotkey").to_owned()),
                    None,
                )
            } else if let Some(slot) = (0..crate::quick_action::MAX_ACTION_HOTKEYS).find(|slot| {
                !self
                    .quick_hotkeys
                    .iter()
                    .any(|old| old.hotkey.slot_contains(*slot))
            }) {
                let mut hotkey = crate::hotkey_registration::HotkeyRegistration::for_quick_action(
                    self.hwnd, slot,
                );
                match hotkey.set(binding) {
                    Ok(_) => {
                        self.quick_hotkeys.push(RegisteredQuickAction {
                            id: action.id.clone(),
                            recipe,
                            hotkey,
                        });
                        (None, Some(spec))
                    }
                    Err(error) => (Some(error.to_string()), None),
                }
            } else {
                (
                    Some(
                        tr!("快捷动作热键槽已满", "Quick action hotkey slots are full").to_owned(),
                    ),
                    None,
                )
            };
            if let Some(error) = failure {
                if let Some(current) = active.find_mut(&action.id) {
                    current.hotkey = actual;
                }
                rollbacks.push((action.id.clone(), action.hotkey, action.enabled, actual));
                errors.push(trf!(
                    "快捷动作 {} 注册失败：{}；保留可用的原绑定。",
                    "Quick action {} registration failed: {}; available previous binding kept.",
                    action.label(),
                    error
                ));
            }
        }
        if !rollbacks.is_empty() {
            let mut latest = crate::quick_action_store::QuickActionStore::load();
            let mut changed = false;
            for (id, expected, enabled, actual) in rollbacks {
                if let Some(action) = latest.find_mut(&id) {
                    // 新一轮设置已经改过同一动作时，不回写过期失败请求。
                    if action.hotkey == expected && action.enabled == enabled {
                        action.hotkey = actual;
                        changed = true;
                    }
                }
            }
            if changed {
                if let Err(error) = latest.save() {
                    errors.push(trf!(
                        "快捷动作配置回退失败：{}",
                        "Quick action rollback failed: {}",
                        error
                    ));
                }
            }
        }
        self.quick_actions = active;
        if !errors.is_empty() {
            balloon(self, &errors.join("；"), true);
        }
    }

    fn capture_parameter(&self, quick: u32) -> usize {
        let quick = if quick == 0 {
            self.quick_actions
                .resolve("capture")
                .ok()
                .and_then(|r| r.pack().ok())
                .unwrap_or(0)
        } else {
            quick
        };
        let mode = crate::quick_action::QuickRecipe::unpack(quick)
            .ok()
            .flatten()
            .map_or(0, |r| match r.source {
                crate::quick_action::Source::CurrentScreen => 1,
                crate::quick_action::Source::AllScreens => 2,
                _ => 0,
            });
        ((quick as usize) << 32) | mode
    }
    fn apply_input_triggers(
        &mut self,
        preferences: &crate::preferences::Preferences,
    ) -> Result<()> {
        let bindings = HotkeyTarget::ALL
            .into_iter()
            .filter_map(|target| {
                preferences
                    .input_trigger_for(target)
                    .map(|trigger| (target, trigger.clone()))
            })
            .collect();
        self.input_triggers.set(self.hwnd, bindings)
    }
    fn hotkey_registration(
        &mut self,
        target: HotkeyTarget,
    ) -> &mut crate::hotkey_registration::HotkeyRegistration {
        &mut self
            .command_hotkeys
            .iter_mut()
            .find(|(candidate, _)| *candidate == target)
            .expect("快捷键命令必须在目录中")
            .1
    }
    fn spawn_pin(&mut self, argument: &str) -> Result<()> {
        self.spawn_pin_args(&[argument.into()])
    }
    fn spawn_pin_args(&mut self, arguments: &[std::ffi::OsString]) -> Result<()> {
        let child = Command::new(std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE))
            .args(arguments)
            .creation_flags(0x08000000)
            .spawn()?;
        unsafe {
            let _ = AllowSetForegroundWindow(child.id());
        }
        self.pin_workers.push(child);
        Ok(())
    }
    fn suspend_hotkeys(&mut self) -> Result<()> {
        let commands: Vec<_> = self
            .command_hotkeys
            .iter()
            .map(|(_, registration)| registration.binding())
            .collect();
        let quick: Vec<_> = self
            .quick_hotkeys
            .iter()
            .map(|action| action.hotkey.binding())
            .collect();
        let result = (|| -> Result<()> {
            for action in &mut self.quick_hotkeys {
                action.hotkey.set(None)?;
            }
            for (_, registration) in &mut self.command_hotkeys {
                registration.set(None)?;
            }
            self.input_triggers.set_enabled(false)?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = self.input_triggers.set_enabled(true);
            for ((target, registration), binding) in self.command_hotkeys.iter_mut().zip(commands) {
                if let Err(restore) = registration.set(binding) {
                    eprintln!("{} 录制取消后恢复失败：{restore}", target.label());
                }
            }
            for (action, binding) in self.quick_hotkeys.iter_mut().zip(quick) {
                if let Err(restore) = action.hotkey.set(binding) {
                    eprintln!("快捷动作 {} 录制取消后恢复失败：{restore}", action.id);
                }
            }
            return Err(error);
        }
        Ok(())
    }
    fn reap_worker(&mut self) -> std::io::Result<bool> {
        let status = match self.worker.as_mut() {
            Some(child) => child.try_wait()?,
            None => None,
        };
        let running = self.worker.is_some() && status.is_none();
        if !running {
            if let Some(status) =
                status.filter(|status| !status.success() && self.worker_is_capture)
            {
                // 详细失败由 worker 留档；通用退出回执不能覆盖其阶段信息。
                if let Some(child) = self.worker.as_ref() {
                    crate::capture_failure::record_worker_exit(child.id(), status.code());
                }
                eprintln!("截图工作进程异常退出：{status}");
                balloon(
                    self,
                    tr!(
                        "截图会话异常结束，请重新触发。",
                        "The capture session ended unexpectedly; please start it again."
                    ),
                    true,
                );
            }
            self.worker = None;
            self.worker_pending_capture = false;
            self.worker_is_capture = false;
            self.standby_submission_trace = None;
        }
        Ok(running)
    }
    fn prepare_standby(&mut self) {
        if self.standby_enabled && self.standby.is_none() {
            match crate::capture_standby::Slot::spawn(self.hwnd) {
                Ok(slot) => self.standby = Some(slot),
                Err(error) => {
                    eprintln!("快速截图待命进程准备失败：{error}");
                    if self.standby_pending.take().is_some() {
                        balloon(
                            self,
                            tr!(
                                "快速截图准备失败，请重新点击托盘截图。",
                                "Capture preparation failed; click the tray icon to capture again."
                            ),
                            true,
                        );
                    }
                }
            }
        }
    }
    fn expire_standby_pending(&mut self, now: Instant) -> bool {
        let unpublished = !self.worker_pending_capture
            && !self
                .standby
                .as_ref()
                .is_some_and(|slot| slot.request_published());
        if expire_standby_pending(&mut self.standby_pending, now, unpublished) {
            balloon(
                self,
                tr!(
                    "截图准备等待超过 10 秒，请重新点击截图。",
                    "Capture preparation took more than 10 seconds; please try again."
                ),
                true,
            );
            true
        } else {
            false
        }
    }
    fn dispatch_standby(&mut self, request: PendingCapture) -> Result<bool> {
        let Some(slot) = self.standby.as_mut() else {
            return Ok(false);
        };
        // 新点击也补查 Ready，不等下一次一秒 Timer；已绑定 Slot 只做廉价原身份核验。
        let requested = match slot
            .poll_ready()
            .and_then(|_| slot.request_quick(request.mode, request.qpc, request.quick))
        {
            Ok(requested) => requested,
            Err(error) => {
                let retry = retry_after_submission_error(request, slot.request_published());
                if retry.is_none() {
                    // 提交后通知失败不能重新截图；保留原 Child 归属直到实际退出。
                    // 不重发 auto-reset request，避免旧 worker 前台门消费重复信号。
                    eprintln!("快速截图请求已发布但通知失败，本次取消且不重投：{error}");
                    crate::capture_failure::record("standby-submit", &error.to_string());
                    if let Err(error) = slot.request_stop() {
                        eprintln!("已发布截图的原 IPC 取消失败：{error}");
                    }
                    self.worker = self.standby.take().map(|slot| slot.child);
                    self.worker_pending_capture = true;
                    self.worker_is_capture = true;
                    self.standby_pending = None;
                    balloon(
                        self,
                        tr!(
                            "本次截图投递失败，请稍后重试。",
                            "This capture couldn't be started; please try again later."
                        ),
                        true,
                    );
                    return Ok(true);
                }
                eprintln!("快速截图待命进程投递失败，重新准备：{error}");
                if let Some(mut slot) = self.standby.take() {
                    if let Err(error) = slot.request_stop() {
                        eprintln!("快速截图待命 IPC 停止失败：{error}");
                    }
                    close_worker(&mut slot.child);
                }
                self.standby_pending = retry;
                self.prepare_standby();
                return Ok(true);
            }
        };
        if requested {
            self.standby_submission_trace = slot.take_submission_trace();
            self.worker = self.standby.take().map(|slot| slot.child);
            self.worker_pending_capture = true;
            self.worker_is_capture = true;
            self.standby_pending = None;
            Ok(true)
        } else {
            // DLL 正在装载时只排队一次，不另起第二个截图进程。
            self.standby_pending = Some(request);
            Ok(true)
        }
    }
}

impl Drop for NativeState {
    fn drop(&mut self) {
        unsafe {
            // 正在装载 DLL、尚无控制窗的待命进程也能观察正常退出请求。
            let _ = SetPropW(
                self.hwnd,
                w!("RustShot.Resident.Closing"),
                Some(HANDLE(std::ptr::dangling_mut())),
            );
            if let Some(slot) = self.standby.as_mut() {
                if let Err(error) = slot.request_stop() {
                    eprintln!("托盘退出时待命 IPC 停止失败：{error}");
                }
                close_worker(&mut slot.child);
            }
            for child in &mut self.pin_workers {
                if matches!(child.try_wait(), Ok(None)) {
                    close_worker(child);
                }
            }
            if let Some(child) = self.worker.as_mut() {
                // 用户退出托盘时同时结束由本次托盘实例创建的截图会话。
                if matches!(child.try_wait(), Ok(None)) {
                    close_worker(child);
                }
            }
            for (_, registration) in &mut self.command_hotkeys {
                let _ = registration.set(None);
            }
            for action in &mut self.quick_hotkeys {
                let _ = action.hotkey.set(None);
            }
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.tray);
            let _ = DestroyWindow(self.hwnd);
            let _ = DestroyIcon(self.tray.hIcon);
            let _ = CloseHandle(self.mutex);
        }
    }
}

fn close_worker(child: &mut Child) {
    struct Request {
        pid: u32,
        has_pins: bool,
    }
    unsafe extern "system" fn close_window(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
        let request = &mut *(data.0 as *mut Request);
        let mut owner = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut owner));
        if owner == request.pid {
            if !GetPropW(hwnd, w!("RustShot.Pin.v1")).is_invalid() {
                request.has_pins = true;
                crate::pin_workspace_runtime::preserve_on_exit(hwnd);
            }
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        windows::core::BOOL(1)
    }
    let mut request = Request {
        pid: child.id(),
        has_pins: false,
    };
    unsafe {
        let _ = EnumWindows(Some(close_window), LPARAM(&mut request as *mut _ as isize));
    }
    // 先给录屏面板机会结束编码；无响应的自有进程才强制清理。
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(if request.has_pins { 10 } else { 2 });
    while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn set_text(dst: &mut [u16], text: &str) {
    dst.fill(0);
    let limit = dst.len().saturating_sub(1);
    for (d, s) in dst.iter_mut().take(limit).zip(text.encode_utf16()) {
        *d = s;
    }
}

fn balloon(native: &NativeState, message: &str, error: bool) {
    unsafe {
        let mut data = native.tray;
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = if error { NIIF_ERROR } else { NIIF_INFO };
        set_text(&mut data.szInfoTitle, crate::brand::NAME);
        set_text(&mut data.szInfo, message);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn tray_icon() -> Result<HICON> {
    // 常驻进程仅加载已由 SVG 生成的原生 ICO，不启动 GPUI 或图像解码库。
    let bytes = include_bytes!("../assets/app.ico");
    let desired = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 64);
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let entry = bytes[6..]
        .chunks_exact(16)
        .take(count)
        .min_by_key(|entry| (i32::from(entry[0]) - desired).abs())
        .ok_or("托盘图标资源为空")?;
    let length = u32::from_le_bytes(entry[8..12].try_into()?) as usize;
    let offset = u32::from_le_bytes(entry[12..16].try_into()?) as usize;
    let image = bytes
        .get(offset..offset + length)
        .ok_or("托盘图标资源无效")?;
    unsafe {
        Ok(CreateIconFromResourceEx(
            image,
            true,
            0x00030000,
            desired,
            desired,
            LR_DEFAULTCOLOR,
        )?)
    }
}

pub fn run() -> Result<()> {
    run_impl(false, false)
}
fn update_tip(native: &mut NativeState, preferences: &crate::preferences::Preferences) {
    set_text(
        &mut native.tray.szTip,
        &trf!(
            "{} · GPUI\n截图 {} · 贴图 {}\n左键区域截图 · 右键打开菜单",
            "{} · GPUI\nCapture {} · Pin {}\nLeft-click to capture · right-click for menu",
            crate::brand::NAME,
            preferences.hotkey_label(),
            preferences.hotkey_label_for(HotkeyTarget::ClipboardPin)
        ),
    );
}

fn reload_hotkey(native: &mut NativeState) {
    let requested = crate::preferences::Preferences::load();
    // 设置保存后会投递 RELOAD；托盘提示与通知随之切换语言。
    crate::i18n::apply(requested.language);
    if native.hotkey_recording.is_some() {
        native.pending_hotkey_reload = true;
        return;
    }
    let previous = native.hotkey_preferences.clone();
    let mut active = requested.clone();
    let mut messages = Vec::new();
    let mut errors = Vec::new();
    let mut failed = Vec::new();
    if let Some((first, second)) = requested.hotkey_conflict_pair() {
        failed.extend(HotkeyTarget::ALL);
        errors.push(trf!(
            "{} 与 {} 的全局快捷键冲突，保留原有组合。",
            "The global shortcuts for {} and {} conflict; previous bindings kept.",
            first.label(),
            second.label()
        ));
        for target in &failed {
            active.restore_hotkey_from(*target, &previous);
        }
    } else {
        for target in HotkeyTarget::ALL {
            let quick_conflict = requested.trigger_for(target).and_then(|trigger| {
                native.quick_hotkeys.iter().find_map(|quick| {
                    let spec = quick_hotkey_spec(quick.hotkey.binding())?;
                    crate::trigger::triggers_conflict(
                        &trigger,
                        &crate::trigger::Trigger::Hotkey(spec),
                    )
                    .then(|| {
                        native
                            .quick_actions
                            .find(&quick.id)
                            .map_or_else(|| quick.id.clone(), |action| action.label().to_owned())
                    })
                })
            });
            if let Some(quick) = quick_conflict {
                errors.push(trf!(
                    "{} 与快捷动作 {} 冲突，保留原有组合。",
                    "{} conflicts with quick action {}; previous binding kept.",
                    target.label(),
                    quick
                ));
                active.restore_hotkey_from(target, &previous);
                failed.push(target);
                continue;
            }
            match native
                .hotkey_registration(target)
                .set(requested.hotkey_for(target))
            {
                Ok(_) => {
                    if !requested.hotkey_matches(target, &previous) {
                        messages.push(format!(
                            "{}：{}",
                            target.label(),
                            requested.hotkey_label_for(target)
                        ));
                    }
                }
                Err(error) => {
                    let registration = native.hotkey_registration(target);
                    if registration.binding().is_none() {
                        let _ = registration.set(previous.hotkey_for(target));
                    }
                    let kept = if registration.binding().is_some() {
                        trf!(
                            "保留原热键 {}",
                            "keeping the previous hotkey {}",
                            previous.hotkey_label_for(target)
                        )
                    } else if previous.hotkey_mode(target) == 4 {
                        tr!("维持快捷键禁用", "the hotkey stays disabled").into()
                    } else {
                        tr!(
                            "当前没有可用热键，可使用托盘菜单",
                            "no hotkey is available; use the tray menu"
                        )
                        .into()
                    };
                    errors.push(trf!(
                        "{} {} 注册失败（{}），{}。",
                        "Failed to register {} {} ({}); {}.",
                        target.label(),
                        requested.hotkey_label_for(target),
                        error,
                        kept
                    ));
                    active.restore_hotkey_from(target, &previous);
                    if registration.binding().is_none()
                        && previous.input_trigger_for(target).is_none()
                    {
                        active.set_hotkey_mode(target, 4);
                    }
                    failed.push(target);
                }
            }
        }
        native.resolve_hotkey_conflicts(&mut active, &previous, &mut failed, &mut errors);
        if let Err(error) = native.apply_input_triggers(&active) {
            // 两种后端之间的切换也要回退，不能留下新系统热键和旧监听同时触发。
            for target in HotkeyTarget::ALL {
                if active.input_trigger_for(target) != previous.input_trigger_for(target) {
                    if let Err(restore) = native
                        .hotkey_registration(target)
                        .set(previous.hotkey_for(target))
                    {
                        errors.push(trf!(
                            "恢复 {} 失败：{}",
                            "Failed to restore {}: {}",
                            target.label(),
                            restore
                        ));
                    }
                    active.restore_hotkey_from(target, &previous);
                    if !failed.contains(&target) {
                        failed.push(target);
                    }
                }
            }
            native.resolve_hotkey_conflicts(&mut active, &previous, &mut failed, &mut errors);
            if let Err(restore) = native.apply_input_triggers(&active) {
                errors.push(trf!(
                    "恢复组合监听失败：{}",
                    "Failed to restore input listening: {}",
                    restore
                ));
            }
            // 回退也可能失败，回执与回写必须反映当前真正可用的绑定。
            for target in failed.iter().copied() {
                if let Some(input) = native.input_triggers.binding(target) {
                    active = active
                        .with_trigger_change(target, crate::trigger::Trigger::Input(input.clone()));
                } else if let Some(spec) =
                    quick_hotkey_spec(native.hotkey_registration(target).binding())
                {
                    active = active.with_hotkey_change(target, 3, Some(spec));
                } else {
                    active.set_hotkey_mode(target, 4);
                }
            }
            errors.push(trf!(
                "键盘／鼠标组合监听失败（{}），已尝试恢复原绑定。",
                "Key/mouse combination listening failed ({}); restoring previous bindings was attempted.",
                error
            ));
        }
    }
    errors.extend(messages);
    let mut text = errors.join("；");
    if text.is_empty() {
        text = tr!("快捷键已生效。", "Shortcuts are active.").into();
    }
    if !failed.is_empty() {
        if let Err(error) =
            crate::preferences::Preferences::revert_hotkeys(&requested, &active, &failed)
        {
            text = format!(
                "{}；{}",
                trf!(
                    "配置回退失败：{error}",
                    "Failed to roll back settings: {error}"
                ),
                text
            );
        }
    }
    native.hotkey_preferences = active;
    native.reload_quick_actions();
    crate::hotkey_status::write(&requested, failed.is_empty(), &text);
    if !failed.is_empty() {
        balloon(native, &text, true);
    }
    let preferences = native.hotkey_preferences.clone();
    update_tip(native, &preferences);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &native.tray);
    }
}
fn resume_hotkey(native: &mut NativeState) {
    native.hotkey_recording = None;
    unsafe {
        let _ = KillTimer(Some(native.hwnd), HOTKEY_RECORD_TIMER);
    }
    // 先恢复录制前的绑定；设置页随后单独投递 RELOAD，才能保留冲突回执。
    let pending = std::mem::take(&mut native.pending_hotkey_reload);
    let preferences = native.hotkey_preferences.clone();
    if let Err(error) = native.input_triggers.set_enabled(true) {
        eprintln!("键盘／鼠标组合监听恢复失败：{error}");
    }
    for target in HotkeyTarget::ALL {
        if let Err(error) = native
            .hotkey_registration(target)
            .set(preferences.hotkey_for(target))
        {
            let message = trf!(
                "恢复 {} {} 失败：{}，可使用托盘菜单。",
                "Failed to restore {} {}: {}; use the tray menu.",
                target.label(),
                preferences.hotkey_label_for(target),
                error
            );
            crate::hotkey_status::write(&preferences, false, &message);
            if !pending {
                balloon(native, &message, true);
            }
        }
    }
    if pending {
        reload_hotkey(native);
    } else {
        native.reload_quick_actions();
    }
}

pub fn idle_probe() -> Result<()> {
    run_impl(true, false)
}
pub fn resident_smoke() -> Result<()> {
    run_impl(false, true)
}

/// 安装、升级和卸载用：向本会话托盘投递 WM_CLOSE，走与托盘菜单“退出”相同的收尾，
/// 等其进程结束。没有托盘时视为已退出；超时返回 false，由调用方决定是否继续等待。
pub fn request_resident_exit(timeout: std::time::Duration) -> Result<bool> {
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    unsafe {
        let Ok(hwnd) = FindWindowW(w!("RustShot.MVP.Window"), None) else {
            return Ok(true);
        };
        if hwnd.0.is_null() {
            return Ok(true);
        }
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let process = if pid == 0 {
            None
        } else {
            OpenProcess(PROCESS_SYNCHRONIZE, false, pid).ok()
        };
        PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0))?;
        let Some(process) = process else {
            return Ok(true);
        };
        let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
        let exited = WaitForSingleObject(process, millis) == WAIT_OBJECT_0;
        let _ = CloseHandle(process);
        Ok(exited)
    }
}

fn run_impl(idle_probe: bool, smoke: bool) -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let action_name = args
        .iter()
        .position(|arg| arg == "--action")
        .map(|at| args.get(at + 1).ok_or("--action 缺少动作 ID"))
        .transpose()?;
    let start_quick = action_name
        .map(|name| {
            crate::quick_action_store::QuickActionStore::load()
                .resolve(name)
                .and_then(|recipe| {
                    recipe
                        .pack()
                        .map_err(crate::quick_action::ActionError::Recipe)
                })
        })
        .transpose()?;
    let capture_on_start = args.iter().any(|arg| arg == "--region") || start_quick.is_some();
    let tray_only = std::env::args().skip(1).any(|arg| arg == "--tray");
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let mutex = CreateMutexW(None, false, w!("Local\\RustShot.MVP"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(mutex);
            if idle_probe || smoke {
                return Err(format!("请先退出 {}，再运行常驻测试", crate::brand::NAME).into());
            }
            let hwnd = FindWindowW(w!("RustShot.MVP.Window"), None)?;
            if !tray_only {
                let mut resident_pid = 0;
                if GetWindowThreadProcessId(hwnd, Some(&mut resident_pid)) == 0 || resident_pid == 0
                {
                    return Err(windows::core::Error::from_win32().into());
                }
                // 本次前台宿主启动的 launcher 将授权传给原 resident；后者再授权 child。
                // 与 Slot.request 一样为 best effort，真实 Ready 仍须确认实际焦点。
                let _ = AllowSetForegroundWindow(resident_pid);
                PostMessageW(
                    Some(hwnd),
                    CAPTURE_MESSAGE,
                    WPARAM((start_quick.unwrap_or(0) as usize) << 32),
                    LPARAM(crate::capture_standby::qpc() as isize),
                )?;
            }
            return Ok(());
        }
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: w!("RustShot.MVP.Window"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            let _ = CloseHandle(mutex);
            return Err(windows::core::Error::from_win32().into());
        }
        // 托盘线程不接收文字；只在本线程首个顶层窗口之前关闭 IME。
        // GPUI 与待命工作进程的主线程保持各自的输入上下文。
        let resident_thread_id = GetCurrentThreadId();
        let resident_ime_disabled = ImmDisableIME(resident_thread_id).as_bool();
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class.lpszClassName,
            w!("RustShot"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(instance),
            None,
        )?;
        let preferences = crate::preferences::Preferences::load();
        SetPropW(
            hwnd,
            w!("RustShot.Resident.Token"),
            Some(HANDLE(crate::capture_standby::qpc() as usize as *mut _)),
        )?;
        SetPropW(
            hwnd,
            w!("RustShot.Resident.IMEAttempted"),
            Some(HANDLE(std::ptr::dangling_mut())),
        )?;
        SetPropW(
            hwnd,
            w!("RustShot.Resident.IMEDisabled"),
            Some(HANDLE(resident_ime_disabled as usize as *mut _)),
        )?;
        SetPropW(
            hwnd,
            w!("RustShot.Resident.IMEThreadId"),
            Some(HANDLE(resident_thread_id as usize as *mut _)),
        )?;
        let mut native = NativeState {
            quick_actions: Default::default(),
            quick_hotkeys: Vec::new(),
            hwnd,
            mutex,
            command_hotkeys: HotkeyTarget::ALL
                .into_iter()
                .map(|target| {
                    (
                        target,
                        crate::hotkey_registration::HotkeyRegistration::for_target(hwnd, target),
                    )
                })
                .collect(),
            input_triggers: Default::default(),
            hotkey_preferences: preferences.clone(),
            hotkey_recording: None,
            pending_hotkey_reload: false,
            last_empty_pin_notice: None,
            modern_tray: false,
            worker: None,
            worker_pending_capture: false,
            worker_is_capture: false,
            pin_workers: Vec::new(),
            startup_pin_restore: resident_pin_restore::Check::default(),
            standby: None,
            standby_pending: None,
            standby_submission_trace: None,
            standby_enabled: !idle_probe
                && !smoke
                && std::env::var("RUSTSHOT_CAPTURE_STANDBY").as_deref() != Ok("0"),
            tray: NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
                uCallbackMessage: tray::NOTIFICATION,
                hIcon: tray_icon()?,
                ..Default::default()
            },
        };
        update_tip(&mut native, &preferences);
        native.modern_tray = tray::add(&native.tray)?;
        let (mut active, conflicts) = startup_hotkey_preferences(&preferences);
        let mut registered = conflicts.is_empty();
        let mut registration_errors: Vec<_> = conflicts
            .into_iter()
            .map(|(earlier, target)| {
                trf!(
                    "{} 与 {} 冲突，{} 本次未启用。",
                    "{} conflicts with {}; {} is inactive for this run.",
                    target.label(),
                    earlier.label(),
                    target.label()
                )
            })
            .collect();
        let mut registration_messages = Vec::new();
        for target in HotkeyTarget::ALL {
            if let Err(error) = native
                .hotkey_registration(target)
                .set(active.hotkey_for(target))
            {
                registered = false;
                active.set_hotkey_mode(target, 4);
                registration_errors.push(trf!(
                    "{} {} 注册失败：{}，可使用托盘菜单。",
                    "Failed to register {} {}: {}; use the tray menu.",
                    target.label(),
                    preferences.hotkey_label_for(target),
                    error
                ));
            }
        }
        if let Err(error) = native.apply_input_triggers(&active) {
            registered = false;
            for target in HotkeyTarget::ALL {
                if active.input_trigger_for(target).is_some() {
                    active.set_hotkey_mode(target, 4);
                }
            }
            registration_errors.push(trf!(
                "键盘／鼠标组合监听启动失败：{}，可使用托盘菜单。",
                "Failed to start key/mouse combination listening: {}; use the tray menu.",
                error
            ));
        }
        for target in HotkeyTarget::ALL {
            if active.trigger_for(target).is_some() {
                registration_messages.push(format!(
                    "{}：{}",
                    target.label(),
                    active.hotkey_label_for(target)
                ));
            }
        }
        registration_errors.extend(registration_messages);
        let registration_message = registration_errors.join("；");
        native.hotkey_preferences = active.clone();
        update_tip(&mut native, &active);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &native.tray);
        native.reload_quick_actions();
        crate::hotkey_status::write(&preferences, registered, &registration_message);
        if !registered {
            balloon(&native, &registration_message, true);
        } else if !tray_only && !capture_on_start && !idle_probe && !smoke {
            // 手动启动时托盘图标常被收进溢出区，提示一次已在后台运行和怎么用。
            balloon(
                &native,
                &trf!(
                    "已在托盘运行。{}；右键托盘图标查看贴图、历史与设置。",
                    "Running in the tray. {}; right-click the tray icon for pins, history and settings.",
                    registration_message
                ),
                false,
            );
        }
        let _ = tray::taskbar_created();
        native.prepare_standby();
        if native.standby_enabled && SetTimer(Some(hwnd), STANDBY_TIMER, 1000, None) == 0 {
            eprintln!("快速截图健康检查计时器不可用");
        }
        if capture_on_start && !idle_probe && !smoke {
            // 首次外部启动也通过托盘的一次性待命入口；加载期间只排队一次。
            PostMessageW(
                Some(hwnd),
                CAPTURE_MESSAGE,
                WPARAM(native.capture_parameter(start_quick.unwrap_or(0))),
                LPARAM(crate::capture_standby::qpc() as isize),
            )?;
        }
        if !idle_probe
            && !smoke
            && std::env::var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY").as_deref() != Ok("1")
        {
            // 不在首屏消息循环前打开贴图库；待命和快捷键可先处理用户请求。
            if SetTimer(
                Some(hwnd),
                resident_pin_restore::TIMER,
                resident_pin_restore::DELAY_MS,
                None,
            ) != 0
            {
                native.startup_pin_restore.schedule();
            } else {
                eprintln!("贴图会话恢复检查计时器不可用");
            }
        }
        if !idle_probe && !smoke {
            if let Err(error) = crate::autostart::repair_moved() {
                eprintln!("开机启动项检查失败：{error}");
            }
        }
        if idle_probe {
            SetTimer(Some(hwnd), 10, 400, None);
        }
        if smoke {
            SetTimer(Some(hwnd), 11, 100, None);
            let (w, p) = memory_mib();
            println!("{{\"phase\":\"resident_before\",\"working_set_mib\":{w:.2},\"private_mib\":{p:.2}}}");
            PostMessageW(Some(hwnd), CAPTURE_MESSAGE, WPARAM(0), LPARAM(0))?;
        }
        let mut completed = 0;
        let mut message = MSG::default();
        // 独立托盘诊断旗只在此处读取；worker 继承它也不会开启首屏前 IO。
        let trace_dispatch = std::env::var_os("RUSTSHOT_TRAY_DISPATCH_TRACE").is_some()
            || std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some();
        let mut dispatch_trace: Option<CaptureDispatchTrace> = None;
        let mut previous_dispatch = None;
        loop {
            let receive_begin = trace_dispatch.then(crate::capture_standby::qpc);
            match GetMessageW(&mut message, None, 0, 0).0 {
                -1 => return Err(windows::core::Error::from_win32().into()),
                0 => break,
                _ => {}
            }
            let received_dispatch = receive_begin.map(|begin| {
                let end = crate::capture_standby::qpc();
                let source_window = message.hwnd.0 as usize;
                let previous = previous_dispatch;
                (begin, end, message.message, source_window, previous)
            });
            let _dispatch_guard =
                received_dispatch
                    .as_ref()
                    .map(
                        |&(begin, end, source, source_window, _)| MessageDispatchGuard {
                            previous: &mut previous_dispatch,
                            current: MessageDispatchTrace {
                                message: source,
                                window: source_window,
                                get_begin: begin,
                                get_end: end,
                                dispatch_begin: crate::capture_standby::qpc(),
                                // Drop 完成后才成为下一条消息的 previous，零值不会输出。
                                dispatch_end: 0,
                            },
                        },
                    );
            // 外层直接消费截图通知；同步 Shell 回调仍由 WindowProc 仅转发事件。
            if matches!(message.message, tray::NOTIFICATION | tray::EVENT) {
                match tray::action(message.wParam, message.lParam, native.modern_tray) {
                    Some(tray::Action::Capture) => {
                        message.message = CAPTURE_MESSAGE;
                        message.wParam = WPARAM(0);
                        message.lParam = LPARAM(crate::capture_standby::qpc() as isize);
                    }
                    Some(tray::Action::Menu(_)) => message.message = tray::EVENT,
                    None => continue,
                }
            }
            // 键盘／鼠标组合命中后与对应热键走同一条执行路径。
            if message.message == crate::trigger_listener::MESSAGE {
                let Some(target) = native.input_triggers.target(message.wParam.0) else {
                    continue;
                };
                // 录制期间或组合刚被改掉时，队列里的旧命中不再执行。
                if native.hotkey_recording.is_some()
                    || native.input_triggers.binding(target).is_none()
                {
                    continue;
                }
                if command_needs_foreground(target) {
                    crate::trigger_listener::claim_foreground();
                }
                if let Some(parameter) = capture_command_parameter(target) {
                    // 本条输入直接进入截图分支，不排队到后续录制暂停消息之后。
                    message.message = CAPTURE_MESSAGE;
                    message.wParam = WPARAM(parameter);
                    message.lParam = LPARAM(crate::capture_standby::qpc() as isize);
                } else {
                    if let Err(error) = native.dispatch_command(target) {
                        balloon(&native, &error.to_string(), true);
                    }
                    continue;
                }
            }
            if message.message == WM_HOTKEY {
                let target = native
                    .command_hotkeys
                    .iter()
                    .find_map(|(target, registration)| {
                        registration
                            .accepts_event(message.wParam.0, message.lParam.0 as usize)
                            .then_some(*target)
                    });
                if let Some(target) = target {
                    if native.hotkey_recording.is_some() {
                        continue;
                    }
                    if let Some(parameter) = capture_command_parameter(target) {
                        message.message = CAPTURE_MESSAGE;
                        message.wParam = WPARAM(parameter);
                        message.lParam = LPARAM(crate::capture_standby::qpc() as isize);
                    } else {
                        if let Err(error) = native.dispatch_command(target) {
                            balloon(&native, &error.to_string(), true);
                        }
                        continue;
                    }
                }
            }
            match message.message {
                EXIT_MESSAGE => break,
                crate::capture_standby::READY => {
                    native.expire_standby_pending(Instant::now());
                    let accepted = match native.standby.as_mut() {
                        Some(slot) => {
                            slot.accept_ready(message.wParam.0, HWND(message.lParam.0 as *mut _))
                        }
                        None => Ok(false),
                    };
                    match accepted {
                        Ok(true) => {
                            native.expire_standby_pending(Instant::now());
                            if let Some(request) =
                                take_ready_pending(true, &mut native.standby_pending)
                            {
                                native.dispatch_standby(request)?;
                            }
                        }
                        Err(error) => {
                            eprintln!("快速截图待命 READY 核验失败，重新准备：{error}");
                            if let Some(mut slot) = native.standby.take() {
                                let _ = slot.request_stop();
                                close_worker(&mut slot.child);
                            }
                            native.prepare_standby();
                        }
                        Ok(false) => {}
                    }
                }
                crate::capture_standby::SURFACE_READY => {
                    // 活跃截图期间不加载下一次 GUI 进程：它的 Windows 启动反馈会
                    // 覆盖框选光标，原生黑表面的 DWM 预热也会争用当前 GPUI 首帧。
                    // 由健康检查计时器在截图互斥体释放后补回一次性待命进程。
                    if native
                        .worker
                        .as_ref()
                        .is_some_and(|child| child.id() as usize == message.wParam.0)
                    {
                        if let Some(trace) = native.standby_submission_trace.take() {
                            trace.emit_after_ready(message.wParam.0);
                        }
                        if let Some(dispatch) = dispatch_trace.take() {
                            // 只在实际首屏之后查询先前 HWND 的当前类；句柄已消失或复用时
                            // 不冒称该信息是先前消息返回瞬间的身份快照。
                            let previous_window_after_ready = dispatch.previous.and_then(|previous| {
                                let window = HWND(previous.window as *mut _);
                                let mut pid = 0;
                                let tid = GetWindowThreadProcessId(window, Some(&mut pid));
                                if tid == 0 || pid != std::process::id() {
                                    return None;
                                }
                                let mut class = [0_u16; 128];
                                let length = GetClassNameW(window, &mut class).max(0) as usize;
                                Some(serde_json::json!({
                                    "hwnd":previous.window,"pid":pid,"tid":tid,
                                    "class":String::from_utf16_lossy(&class[..length]),
                                    "query_scope":"current own window after actual Ready; not past identity attestation"
                                }))
                            });
                            crate::startup_probe::emit_trace_value(&serde_json::json!({
                                "phase":"tray_notification_dispatch", "pid":std::process::id(),
                                "worker_pid":message.wParam.0,"request_qpc":dispatch.request_qpc,
                                "resident_thread_id":resident_thread_id,
                                "resident_thread_ime_disabled":resident_ime_disabled,
                                "source_message":dispatch.source_message,
                                "source_window":dispatch.source_window,
                                "previous_message_id":dispatch.previous.map(|previous| previous.message),
                                "previous_message_get_begin_qpc":dispatch.previous.map(|previous| previous.get_begin),
                                "previous_message_get_return_qpc":dispatch.previous.map(|previous| previous.get_end),
                                "previous_message_dispatch_begin_qpc":dispatch.previous.map(|previous| previous.dispatch_begin),
                                "previous_message_dispatch_end_qpc":dispatch.previous.map(|previous| previous.dispatch_end),
                                "previous_message_window":dispatch.previous.map(|previous| previous.window),
                                "previous_window_after_ready":previous_window_after_ready,
                                "surface_ready_received_qpc":crate::capture_standby::qpc(),
                                "get_message_begin_qpc":dispatch.begin, "get_message_return_qpc":dispatch.end,
                                "emitted_after_actual_surface_ready":true
                            }));
                        }
                    }
                }
                WM_TIMER if message.wParam.0 == resident_pin_restore::TIMER => {
                    let _ = KillTimer(Some(hwnd), resident_pin_restore::TIMER);
                    if native.startup_pin_restore.begin() {
                        if let Err(error) = resident_pin_restore::start_check(hwnd) {
                            native.startup_pin_restore.complete(false);
                            eprintln!("贴图会话恢复检查启动失败：{error}");
                        }
                    }
                }
                resident_pin_restore::READY => {
                    if native.startup_pin_restore.complete(message.wParam.0 == 1) {
                        let child = std::env::current_exe().and_then(|path| {
                            Command::new(path.with_file_name(crate::brand::GPUI_EXE))
                                .arg("--pin-session-restore")
                                .creation_flags(0x08000000)
                                .spawn()
                        });
                        match child {
                            Ok(child) => native.pin_workers.push(child),
                            Err(error) => eprintln!("贴图会话恢复启动失败：{error}"),
                        }
                    }
                }
                WM_TIMER if message.wParam.0 == STANDBY_TIMER => {
                    // 请求消费后即使尚未报告开始就退出，也要在闲置时补回待命进程。
                    // 查询失败保留原 Child，不能让局部系统错误结束托盘或遗失进程归属。
                    if let Err(error) = native.reap_worker() {
                        eprintln!("截图进程状态检查失败：{error}");
                    }
                    native.expire_standby_pending(Instant::now());
                    if native
                        .standby
                        .as_mut()
                        .is_some_and(|slot| matches!(slot.child.try_wait(), Ok(Some(_))))
                    {
                        native.standby = None;
                    }
                    if !native.worker_pending_capture {
                        let busy = OpenMutexW(
                            SYNCHRONIZATION_ACCESS_RIGHTS(0x00100000),
                            false,
                            w!("Local\\RustShot.MVP.Capture"),
                        );
                        if let Ok(handle) = busy {
                            let _ = CloseHandle(handle);
                        } else {
                            native.prepare_standby();
                            // READY 只通知一次；按固定原 Slot 的元数据补绑，避免 pending
                            // 永久挡住后续点击。探测错误保留原 Child，不杀进程或清除请求。
                            let accepted = match native.standby.as_mut() {
                                Some(slot) => slot.poll_ready(),
                                None => Ok(false),
                            };
                            match accepted {
                                Ok(ready) => {
                                    native.expire_standby_pending(Instant::now());
                                    if let Some(request) =
                                        take_ready_pending(ready, &mut native.standby_pending)
                                    {
                                        native.dispatch_standby(request)?;
                                    }
                                }
                                Err(error) => {
                                    eprintln!("快速截图待命 READY 补查失败，保留原进程：{error}");
                                }
                            }
                        }
                    }
                }
                CAPTURE_STARTED_MESSAGE => {
                    // 旧会话或直接 CLI 工作进程的通知不能解除新工作进程的准备状态。
                    if native
                        .worker
                        .as_ref()
                        .is_some_and(|child| child.id() as usize == message.wParam.0)
                    {
                        native.worker_pending_capture = false;
                    }
                }
                tray::RESTORE => match tray::add(&native.tray) {
                    Ok(modern) => native.modern_tray = modern,
                    Err(e) => balloon(&native, &e.to_string(), true),
                },
                RELOAD_MESSAGE => reload_hotkey(&mut native),
                crate::trigger_listener::ERROR_MESSAGE => {
                    let errors = native.input_triggers.take_errors();
                    if !errors.is_empty() {
                        balloon(
                            &native,
                            &trf!(
                                "键盘／鼠标组合监听：{}",
                                "Key/mouse combination listening: {}",
                                errors.join("；")
                            ),
                            true,
                        );
                    }
                }
                crate::quick_toast::MESSAGE => {
                    if let Err(error) = crate::quick_toast::show(
                        hwnd,
                        crate::preferences::Preferences::load().theme,
                    ) {
                        eprintln!("快捷动作提示失败：{error}");
                    }
                }
                HOTKEY_RECORD_MESSAGE => {
                    let owner = message.wParam.0 as u32;
                    let token = message.lParam.0;
                    let target = HWND(token.unsigned_abs() as *mut _);
                    if token > 0 {
                        let mut actual = 0;
                        GetWindowThreadProcessId(target, Some(&mut actual));
                        let mut title = [0_u16; 80];
                        let size = GetWindowTextW(target, &mut title);
                        if actual == owner
                            && GetForegroundWindow() == target
                            // 主界面标题为品牌名；旧版设置窗标题随界面语言变化，一并接受。
                            && [
                                crate::brand_name!(),
                                concat!(crate::brand_name!(), " · 设置"),
                                concat!(crate::brand_name!(), " · Settings"),
                            ]
                            .contains(&String::from_utf16_lossy(&title[..size.max(0) as usize]).as_str())
                            && native.suspend_hotkeys().is_ok()
                        {
                            native.hotkey_recording = Some((owner, target));
                            SetTimer(Some(hwnd), HOTKEY_RECORD_TIMER, 250, None);
                        }
                    } else if native.hotkey_recording == Some((owner, target)) {
                        resume_hotkey(&mut native);
                    }
                }
                WM_TIMER if message.wParam.0 == HOTKEY_RECORD_TIMER => {
                    if native.hotkey_recording.is_some_and(|(owner, target)| {
                        let mut actual = 0;
                        GetWindowThreadProcessId(target, Some(&mut actual));
                        actual != owner
                            || !IsWindow(Some(target)).as_bool()
                            || GetForegroundWindow() != target
                    }) {
                        resume_hotkey(&mut native);
                    }
                }
                PIN_MESSAGE if message.wParam.0 as u32 == tray::PIN_REQUEST => {
                    for request in crate::tray_request::take() {
                        if let Err(error) = native.spawn_pin_args(&request.args()) {
                            balloon(
                                &native,
                                &trf!("贴图启动失败：{error}", "Failed to start the pin: {error}"),
                                true,
                            );
                        }
                    }
                }
                PIN_MESSAGE => {
                    let argument = match message.wParam.0 as u32 {
                        tray::PIN_FILE => "--pin-file",
                        tray::PIN_RESTORE => "--pin-restore",
                        tray::PIN_WORKSPACE => "--pin-workspace",
                        _ => "--pin-clipboard",
                    };
                    if let Err(error) = native.spawn_pin(argument) {
                        balloon(
                            &native,
                            &trf!("贴图启动失败：{error}", "Failed to start the pin: {error}"),
                            true,
                        );
                    }
                }
                WM_HOTKEY | CAPTURE_MESSAGE => {
                    let parameter = if message.message == WM_HOTKEY {
                        if let Some(quick) = native.quick_hotkeys.iter().find(|a| {
                            a.hotkey
                                .accepts_event(message.wParam.0, message.lParam.0 as usize)
                        }) {
                            native.capture_parameter(quick.recipe)
                        } else {
                            continue;
                        }
                    } else if message.wParam.0 & 0xf == 0 {
                        native.capture_parameter((message.wParam.0 >> 32) as u32)
                    } else {
                        message.wParam.0
                    };
                    let quick = (parameter >> 32) as u32;
                    if crate::quick_action::QuickRecipe::unpack(quick).is_err() {
                        continue;
                    }
                    native.expire_standby_pending(Instant::now());
                    if native.standby_pending.is_some() {
                        continue;
                    }
                    let running = match native.reap_worker() {
                        Ok(running) => running,
                        Err(error) => {
                            balloon(
                                &native,
                                &trf!(
                                    "截图进程状态检查失败：{error}",
                                    "Failed to check the capture process: {error}"
                                ),
                                true,
                            );
                            continue;
                        }
                    };
                    native
                        .pin_workers
                        .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
                    // 子进程尚在 EXE/驱动准备阶段时未建立互斥体；它仍是截图，不是贴图。
                    if running && native.worker_pending_capture {
                        continue;
                    }
                    let capture_busy = match OpenMutexW(
                        SYNCHRONIZATION_ACCESS_RIGHTS(0x00100000),
                        false,
                        w!("Local\\RustShot.MVP.Capture"),
                    ) {
                        Ok(handle) => {
                            let _ = CloseHandle(handle);
                            true
                        }
                        Err(_) => false,
                    };
                    let launch =
                        worker_launch(running, native.worker_pending_capture, capture_busy);
                    if launch == WorkerLaunch::DetachAndSpawn {
                        if let Some(child) = native.worker.take() {
                            native.pin_workers.push(child);
                        }
                        native.worker_pending_capture = false;
                        native.worker_is_capture = false;
                    }
                    if launch != WorkerLaunch::Wait {
                        let executable =
                            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE);
                        let argument = worker_argument(CAPTURE_MESSAGE, parameter, smoke);
                        let qpc = if message.message == CAPTURE_MESSAGE && message.lParam.0 > 0 {
                            message.lParam.0 as i64
                        } else {
                            crate::capture_standby::qpc()
                        };
                        if !smoke && starts_capture(argument) {
                            let mode = match argument {
                                "--fullscreen" => 1,
                                "--all-screens" => 2,
                                _ => 0,
                            };
                            let mut request = PendingCapture::new(mode, qpc, Instant::now());
                            request.quick = quick;
                            if native.dispatch_standby(request)? {
                                // 只有本次请求已派发或被 standby_pending 接受，才保存其区间。
                                // 被 active/pending guard 忽略的后续通知不能覆盖它。
                                dispatch_trace = received_dispatch.map(
                                    |(begin, end, source, source_window, previous)| {
                                        CaptureDispatchTrace {
                                            begin,
                                            end,
                                            request_qpc: qpc,
                                            source_message: source,
                                            source_window,
                                            previous,
                                        }
                                    },
                                );
                                continue;
                            }
                        }
                        match Command::new(executable)
                            .arg(argument)
                            .args(if quick == 0 {
                                Vec::new()
                            } else {
                                vec!["--quick".to_string(), format!("0x{quick:08X}")]
                            })
                            .env("RUSTSHOT_STARTUP_REQUEST_QPC", qpc.to_string())
                            .creation_flags(0x08000000)
                            .spawn()
                        {
                            Ok(child) => {
                                let _ = AllowSetForegroundWindow(child.id());
                                native.worker_pending_capture = starts_capture(argument);
                                native.worker_is_capture = starts_capture(argument);
                                native.worker = Some(child);
                                if starts_capture(argument) {
                                    dispatch_trace = received_dispatch.map(
                                        |(begin, end, source, source_window, previous)| {
                                            CaptureDispatchTrace {
                                                begin,
                                                end,
                                                request_qpc: qpc,
                                                source_message: source,
                                                source_window,
                                                previous,
                                            }
                                        },
                                    );
                                }
                            }
                            Err(e) if smoke => return Err(e.into()),
                            Err(e) => balloon(
                                &native,
                                &trf!(
                                    "无法启动 GPUI 截图：{e}",
                                    "Cannot start the GPUI capture: {e}"
                                ),
                                true,
                            ),
                        }
                    }
                }
                tray::EVENT => {
                    match tray::action(message.wParam, message.lParam, native.modern_tray) {
                        Some(tray::Action::Capture) => {
                            PostMessageW(
                                Some(hwnd),
                                CAPTURE_MESSAGE,
                                WPARAM(0),
                                LPARAM(crate::capture_standby::qpc() as isize),
                            )?;
                        }
                        Some(tray::Action::Menu(anchor)) => match tray::show_menu(hwnd, anchor)? {
                            id if id >= tray::QUICK_ACTION_BASE
                                && id < tray::QUICK_ACTION_BASE + 64 =>
                            {
                                let store = crate::quick_action_store::QuickActionStore::load();
                                if let Some(action) = store
                                    .actions
                                    .get((id - tray::QUICK_ACTION_BASE) as usize)
                                    .filter(|a| a.enabled && a.in_tray)
                                {
                                    let quick = action.recipe.pack()?;
                                    PostMessageW(
                                        Some(hwnd),
                                        CAPTURE_MESSAGE,
                                        WPARAM(native.capture_parameter(quick)),
                                        LPARAM(0),
                                    )?;
                                }
                            }
                            id if menu_command(id).is_some() => {
                                if let Err(error) =
                                    native.dispatch_command(menu_command(id).unwrap())
                                {
                                    balloon(&native, &error.to_string(), true);
                                }
                            }
                            mode @ (tray::PIN_HIDE | tray::PIN_SHOW | tray::PIN_CLOSE) => {
                                let owners = native.pin_owners();
                                use crate::pin_native::Manage;
                                let action = match mode {
                                    tray::PIN_HIDE => Manage::Hide,
                                    tray::PIN_SHOW => Manage::Show,
                                    _ => Manage::Close,
                                };
                                match crate::pin_workspace_runtime::manage(&owners, action) {
                                    Ok(count)
                                        if count + crate::pin_native::manage(&owners, action)
                                            == 0 =>
                                    {
                                        balloon(
                                            &native,
                                            tr!(
                                                "当前没有可操作的贴图。",
                                                "There are no pins to act on."
                                            ),
                                            false,
                                        );
                                    }
                                    Err(error) => balloon(&native, &error.to_string(), true),
                                    _ => {}
                                }
                            }
                            tray::HELP => {
                                let args = ["--settings", "--page", "help"].map(Into::into);
                                if let Err(error) = native.spawn_pin_args(&args) {
                                    balloon(&native, &error.to_string(), true);
                                }
                            }
                            tray::EXIT => break,
                            _ => {}
                        },
                        None => {}
                    }
                }
                WM_TIMER if idle_probe => {
                    let (w, p) = memory_mib();
                    println!("{{\"phase\":\"resident_idle\",\"working_set_mib\":{w:.2},\"private_mib\":{p:.2}}}");
                    break;
                }
                WM_TIMER if smoke => {
                    if let Some(child) = native.worker.as_mut() {
                        if let Some(status) = child.try_wait()? {
                            if !status.success() {
                                return Err("GPUI 截图工作进程测试失败".into());
                            }
                            native.worker = None;
                            native.worker_pending_capture = false;
                            native.worker_is_capture = false;
                            completed += 1;
                            let (w, p) = memory_mib();
                            println!("{{\"phase\":\"resident_after_worker\",\"iteration\":{completed},\"working_set_mib\":{w:.2},\"private_mib\":{p:.2}}}");
                            if completed == 5 {
                                break;
                            }
                            PostMessageW(Some(hwnd), CAPTURE_MESSAGE, WPARAM(0), LPARAM(0))?;
                        }
                    }
                }
                _ => {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sent_shell_notifications_are_forwarded_to_the_state_loop() {
        unsafe {
            let instance: HINSTANCE = GetModuleHandleW(None).unwrap().into();
            let name = w!("RustShot.Test.SentNotifications");
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: name,
                ..Default::default()
            };
            assert_ne!(RegisterClassW(&class), 0);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                name,
                w!("RustShot callback test"),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(instance),
                None,
            )
            .unwrap();
            let wp = WPARAM((120u32 << 16 | 80u32) as usize);
            let lp = LPARAM(((1u32 << 16) | NIN_SELECT) as isize);
            SendMessageW(hwnd, tray::NOTIFICATION, Some(wp), Some(lp));
            let mut message = MSG::default();
            assert!(PeekMessageW(
                &mut message,
                Some(hwnd),
                tray::EVENT,
                tray::EVENT,
                PM_REMOVE
            )
            .as_bool());
            assert_eq!(message.wParam, wp);
            assert_eq!(message.lParam, lp);
            SendMessageW(hwnd, tray::taskbar_created(), None, None);
            assert!(PeekMessageW(
                &mut message,
                Some(hwnd),
                tray::RESTORE,
                tray::RESTORE,
                PM_REMOVE
            )
            .as_bool());
            DestroyWindow(hwnd).unwrap();
            UnregisterClassW(name, Some(instance)).unwrap();
        }
    }
}

#[cfg(test)]
mod standby_ready_tests {
    use super::*;

    #[test]
    fn expired_pending_allows_new_click_without_auto_capture_and_keeps_fresh_request() {
        let queued_at = Instant::now();
        let mut pending = None;
        for ready in [false, true, true, false] {
            assert_eq!(take_ready_pending(ready, &mut pending), None);
            assert_eq!(pending, None);
        }
        let original = PendingCapture::new(1, (1_i64 << 50) + 819, queued_at);
        pending = Some(original);
        for _ in 0..3 {
            assert_eq!(take_ready_pending(false, &mut pending), None);
            assert_eq!(pending, Some(original));
        }
        assert!(!expire_standby_pending(
            &mut pending,
            queued_at + STANDBY_QUEUE_TIMEOUT - Duration::from_millis(1),
            true,
        ));
        assert_eq!(pending, Some(original));
        assert!(expire_standby_pending(
            &mut pending,
            queued_at + STANDBY_QUEUE_TIMEOUT,
            true,
        ));
        assert_eq!(take_ready_pending(true, &mut pending), None);
        // 原 pending 守卫现在放行新的点击；新 QPC/入队时刻来自新请求。
        let fresh = PendingCapture::new(2, original.qpc + 997, queued_at + STANDBY_QUEUE_TIMEOUT);
        pending = Some(fresh);
        assert!(!expire_standby_pending(&mut pending, fresh.queued_at, true));
        assert_eq!(take_ready_pending(true, &mut pending), Some(fresh));
    }

    #[test]
    fn timer_and_ready_serial_orders_take_each_original_mode_and_qpc_only_once() {
        let queued_at = Instant::now();
        for mode in 0..=2 {
            for notifications in [["timer", "ready"], ["ready", "timer"]] {
                let original = PendingCapture::new(mode, (1_i64 << 50) + mode as i64, queued_at);
                let mut pending = Some(original);
                let mut requests = Vec::new();
                for _notification in notifications {
                    assert!(!expire_standby_pending(
                        &mut pending,
                        queued_at + Duration::from_millis(9999),
                        true,
                    ));
                    if let Some(request) = take_ready_pending(true, &mut pending) {
                        requests.push(request);
                    }
                }
                assert_eq!(requests, [original]);
                assert_eq!(pending, None);
                // READY 重复到达或下一次 Timer 不重新生成 mode/QPC。
                assert_eq!(take_ready_pending(true, &mut pending), None);
                // 同样的顺序遇到过期旧点击，一次请求也不能产生。
                pending = Some(original);
                for _notification in notifications {
                    expire_standby_pending(&mut pending, queued_at + STANDBY_QUEUE_TIMEOUT, true);
                    assert_eq!(take_ready_pending(true, &mut pending), None);
                }
            }
        }
    }

    #[test]
    fn published_submission_error_never_requeues_or_expires_active_request() {
        let queued_at = Instant::now();
        let original = PendingCapture::new(0, (1_i64 << 50) + 19, queued_at);
        assert_eq!(retry_after_submission_error(original, true), None);
        // 错误发生在提交前，仍是同一个请求，绝不续期或修改原 QPC。
        let mut retry = retry_after_submission_error(original, false);
        assert_eq!(retry, Some(original));
        assert!(expire_standby_pending(
            &mut retry,
            queued_at + STANDBY_QUEUE_TIMEOUT,
            true,
        ));
        let mut active = Some(original);
        assert!(!expire_standby_pending(
            &mut active,
            queued_at + Duration::from_secs(600),
            false,
        ));
        assert_eq!(active, Some(original));
    }
}

#[cfg(test)]
mod command_tests {
    #[test]
    fn failed_system_change_reverts_new_hook_that_conflicts_with_retained_key() {
        use crate::hotkey::{HotkeySpec, CTRL};
        use crate::trigger::{InputTrigger, Trigger};
        let previous = crate::preferences::Preferences::default().with_hotkey_change(
            HotkeyTarget::Capture,
            3,
            Some(HotkeySpec::from_key_name("F1", CTRL).unwrap()),
        );
        let requested = previous
            .with_hotkey_change(
                HotkeyTarget::Capture,
                3,
                Some(HotkeySpec::from_key_name("F2", CTRL).unwrap()),
            )
            .with_trigger_change(
                HotkeyTarget::PinToggleGroup,
                Trigger::Input(InputTrigger::from_wire("key:70 + key:72").unwrap()),
            );
        assert!(!requested.hotkeys_conflict());
        // 系统拒绝新 Ctrl+F2 后，Ctrl+F1 仍被原命令持有。
        let mut active = requested.clone();
        active.restore_hotkey_from(HotkeyTarget::Capture, &previous);
        assert!(active.hotkeys_conflict());
        let reverted = rollback_conflicting_changes(&mut active, &previous);
        assert_eq!(reverted, vec![HotkeyTarget::PinToggleGroup]);
        assert!(!active.hotkeys_conflict());
        assert_eq!(
            active.trigger_for(HotkeyTarget::Capture),
            previous.trigger_for(HotkeyTarget::Capture)
        );
        assert!(active.trigger_for(HotkeyTarget::PinToggleGroup).is_none());
    }

    #[test]
    fn startup_system_and_input_conflicts_do_not_activate_together() {
        let requested = crate::preferences::Preferences::default()
            .with_trigger_change(
                HotkeyTarget::PinToggleGroup,
                crate::trigger::Trigger::Input(
                    crate::trigger::InputTrigger::from_wire("key:14 + key:31").unwrap(),
                ),
            )
            .with_hotkey_change(
                HotkeyTarget::History,
                3,
                Some(HotkeyTarget::Capture.default_spec()),
            )
            .with_trigger_change(
                HotkeyTarget::PinNextGroup,
                crate::trigger::Trigger::Input(
                    crate::trigger::InputTrigger::from_wire("key:14 + key:4e").unwrap(),
                ),
            );
        let (active, conflicts) = startup_hotkey_preferences(&requested);
        assert_eq!(conflicts.len(), 2);
        assert!(active.trigger_for(HotkeyTarget::Capture).is_some());
        assert!(active.trigger_for(HotkeyTarget::ClipboardPin).is_some());
        assert!(active.trigger_for(HotkeyTarget::PinToggleGroup).is_none());
        assert!(active.trigger_for(HotkeyTarget::History).is_none());
        assert!(active.trigger_for(HotkeyTarget::PinNextGroup).is_some());
        assert!(!active.hotkeys_conflict());
        // 临时注册失败不能删除用户期望值，后续可在释放占用后重新应用。
        assert!(requested
            .trigger_for(HotkeyTarget::PinToggleGroup)
            .is_some());
        assert!(requested.trigger_for(HotkeyTarget::History).is_some());
    }

    #[test]
    fn group_commands_do_not_capture_or_claim_foreground() {
        for command in [
            HotkeyTarget::PinToggleGroup,
            HotkeyTarget::PinNextGroup,
            HotkeyTarget::PinPreviousGroup,
            HotkeyTarget::PinInteract,
        ] {
            assert_eq!(capture_command_parameter(command), None);
            assert!(!command_needs_foreground(command));
        }
        assert_eq!(
            menu_command(tray::PIN_TOGGLE_GROUP),
            Some(HotkeyTarget::PinToggleGroup)
        );
        assert_eq!(menu_command(tray::PIN_CLOSE), None);
        assert_eq!(
            capture_command_parameter(HotkeyTarget::CurrentScreen),
            Some(1)
        );
        assert_eq!(capture_command_parameter(HotkeyTarget::AllScreens), Some(2));
    }
    #[test]
    fn quick_action_hotkey_modifiers_round_trip_through_windows_bits() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_ALT, MOD_CONTROL, MOD_NOREPEAT};
        for modifiers in 0..16 {
            let spec = crate::hotkey::HotkeySpec {
                modifiers,
                key: 0x70,
            };
            let native = super::quick_native_binding(spec);
            assert_eq!(super::quick_hotkey_spec(Some(native)), Some(spec));
            assert!(native.0.contains(MOD_NOREPEAT));
        }
        let control = super::quick_native_binding(crate::hotkey::HotkeySpec {
            modifiers: crate::hotkey::CTRL,
            key: 0x70,
        });
        assert!(control.0.contains(MOD_CONTROL));
        assert!(!control.0.contains(MOD_ALT));
    }
    use super::*;
    #[test]
    fn hotkey_identifier_does_not_become_fullscreen_mode() {
        assert_eq!(
            worker_argument(WM_HOTKEY, HOTKEY as usize, false),
            "--region"
        );
        assert_eq!(worker_argument(CAPTURE_MESSAGE, 1, false), "--fullscreen");
        assert_eq!(worker_argument(CAPTURE_MESSAGE, 2, false), "--all-screens");
        assert_eq!(worker_argument(CAPTURE_MESSAGE, 4, false), "--history");
        assert_eq!(worker_argument(CAPTURE_MESSAGE, 8, false), "--settings");
    }
    #[test]
    fn preparing_capture_is_not_detached_as_pin_but_auxiliary_pin_and_exit_allow_next_capture() {
        for argument in ["--region", "--fullscreen", "--all-screens", "--smoke"] {
            assert!(starts_capture(argument));
            assert_eq!(
                worker_launch(true, starts_capture(argument), false),
                WorkerLaunch::Wait
            );
        }
        for mode in [4, 8] {
            let argument = worker_argument(CAPTURE_MESSAGE, mode, false);
            assert!(!starts_capture(argument));
            assert_eq!(
                worker_launch(true, starts_capture(argument), false),
                WorkerLaunch::DetachAndSpawn
            );
        }
        // 通知已清准备状态，但互斥体仍存在：尚未输出的截图不能并发启动。
        assert_eq!(worker_launch(true, false, true), WorkerLaunch::Wait);
        // 截图转贴图释放互斥体后，旧进程继续存活且被保存到 pin_workers。
        assert_eq!(
            worker_launch(true, false, false),
            WorkerLaunch::DetachAndSpawn
        );
        // 通知到达前工作进程失败退出，下一次点击不能被遗留 pending 阻塞。
        assert_eq!(worker_launch(false, true, false), WorkerLaunch::Spawn);
        assert_eq!(worker_launch(false, false, true), WorkerLaunch::Wait);
    }
}
