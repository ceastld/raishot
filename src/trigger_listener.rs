//! 常驻进程的键盘／鼠标组合监听。只有配置了组合时才安装低级输入 Hook，全部改回系统热键
//! 或停用后立即卸载。Hook 运行在专用线程，只做有界匹配；命中后投递消息给托盘窗口，
//! 由外层消息循环按原热键路径执行动作。
use crate::{hotkey::HotkeyTarget, trigger::InputTrigger, Result};
use omnitrigger_windows::{Backend, BackendCommand, BackendEvent};
use parking_lot::Mutex;
use std::{mem::size_of, sync::Arc};
use windows::Win32::{
    Foundation::*,
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};

/// wParam 同时包含绑定代次与命令编号，旧绑定的排队命中不会执行新命令。
pub(crate) const MESSAGE: u32 = WM_APP + 24;
/// 监听线程报告的错误已放入 [`TriggerListener::take_errors`]。
pub(crate) const ERROR_MESSAGE: u32 = WM_APP + 25;
const ERROR_LIMIT: usize = 8;

fn id(target: HotkeyTarget) -> u64 {
    HotkeyTarget::ALL
        .iter()
        .position(|candidate| *candidate == target)
        .expect("快捷键命令必须在目录中") as u64
        + 1
}

fn target(id: usize) -> Option<HotkeyTarget> {
    id.checked_sub(1)
        .and_then(|index| HotkeyTarget::ALL.get(index))
        .copied()
}

#[derive(Default)]
pub(crate) struct TriggerListener {
    backend: Option<Backend>,
    active: Vec<(HotkeyTarget, InputTrigger)>,
    generation: u64,
    errors: Arc<Mutex<Vec<String>>>,
}

impl TriggerListener {
    pub(crate) fn target(&self, event: usize) -> Option<HotkeyTarget> {
        if (event as u64 >> 8) != self.generation {
            return None;
        }
        target(event & 0xff).filter(|target| self.binding(*target).is_some())
    }

    pub(crate) fn binding(&self, target: HotkeyTarget) -> Option<&InputTrigger> {
        self.active
            .iter()
            .find(|(active, _)| *active == target)
            .map(|(_, trigger)| trigger)
    }

    /// 整组替换；失败时保留原监听与原绑定。
    pub(crate) fn set(
        &mut self,
        window: HWND,
        bindings: Vec<(HotkeyTarget, InputTrigger)>,
    ) -> Result<()> {
        if bindings == self.active {
            return Ok(());
        }
        if bindings.is_empty() {
            // 退出监听线程会卸载 Hook；已消费的按压最多等待两秒松开。
            self.backend = None;
            self.active.clear();
            self.generation = self.generation.wrapping_add(1);
            return Ok(());
        }
        if crate::trigger::input_triggers_conflict(bindings.iter().map(|(_, t)| t)) {
            return Err(tr!(
                "键盘／鼠标组合相互冲突：一个组合的触发键是另一个组合按住的键",
                "The key/mouse combinations conflict: one combination's trigger key is held by another"
            )
            .into());
        }
        let generation = self.generation.wrapping_add(1).max(1);
        let specs = bindings
            .iter()
            .map(|(target, trigger)| trigger.spec((generation << 8) | id(*target)))
            .collect();
        if self.backend.is_none() {
            self.backend = Some(self.start(window)?);
        }
        let backend = self.backend.as_ref().expect("监听已启动");
        backend.send(BackendCommand::SetBindings(specs))?;
        self.active = bindings;
        self.generation = generation;
        Ok(())
    }

    /// 录制新快捷键期间暂停匹配，避免旧组合抢先执行。
    pub(crate) fn set_enabled(&mut self, enabled: bool) -> Result<()> {
        if let Some(backend) = &self.backend {
            // 暂停前已排队的事件在恢复后也不得执行；恢复时重新给绑定分配代次。
            self.generation = self.generation.wrapping_add(1).max(1);
            if enabled {
                let specs = self
                    .active
                    .iter()
                    .map(|(target, trigger)| trigger.spec((self.generation << 8) | id(*target)))
                    .collect();
                backend.send(BackendCommand::SetBindings(specs))?;
            }
            backend.send(BackendCommand::SetEnabled(enabled))?;
        }
        Ok(())
    }

    pub(crate) fn take_errors(&self) -> Vec<String> {
        std::mem::take(&mut *self.errors.lock())
    }

    fn start(&self, window: HWND) -> Result<Backend> {
        let (backend, events) = Backend::start()?;
        let window = window.0 as usize;
        let errors = self.errors.clone();
        // 通道随监听线程结束而关闭，转发线程随之退出。
        std::thread::Builder::new()
            .name("rustshot-trigger-events".into())
            .spawn(move || {
                let post = |message: u32, parameter: usize| unsafe {
                    let _ = PostMessageW(
                        Some(HWND(window as *mut _)),
                        message,
                        WPARAM(parameter),
                        LPARAM(0),
                    );
                };
                for event in events {
                    match event {
                        BackendEvent::Triggered(id) => post(MESSAGE, id as usize),
                        BackendEvent::Error(message) => {
                            let mut queue = errors.lock();
                            if queue.len() < ERROR_LIMIT {
                                queue.push(message);
                            }
                            drop(queue);
                            post(ERROR_MESSAGE, 0);
                        }
                        _ => {}
                    }
                }
            })?;
        Ok(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_event_must_match_current_binding_and_generation() {
        let command = HotkeyTarget::PinToggleGroup;
        let mut listener = TriggerListener {
            active: vec![(command, InputTrigger::from_wire("key:14 + key:48").unwrap())],
            generation: 7,
            ..Default::default()
        };
        let event = ((7 << 8) | id(command)) as usize;
        assert_eq!(listener.target(event), Some(command));
        assert_eq!(listener.target(((6 << 8) | id(command)) as usize), None);
        assert_eq!(
            listener.target(((7 << 8) | id(HotkeyTarget::Capture)) as usize),
            None
        );
        listener.active.clear();
        assert_eq!(listener.target(event), None);
    }
}

/// 组合被监听消费后系统不会像热键那样授予前台权限；补发一次零位移鼠标输入，让本进程
/// 成为最近的输入来源，随后才能把前台权限转交给截图／贴图进程。
pub(crate) fn claim_foreground() {
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dwFlags: MOUSEEVENTF_MOVE,
                ..Default::default()
            },
        },
    };
    unsafe {
        SendInput(&[input], size_of::<INPUT>() as i32);
    }
}
