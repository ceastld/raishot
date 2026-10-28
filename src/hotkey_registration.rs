//! 先取得新组合，再释放旧组合；注册失败时旧热键仍然可用。
use crate::hotkey::HotkeyTarget;
use crate::Result;
use windows::Win32::{Foundation::HWND, UI::Input::KeyboardAndMouse::*};

#[derive(Clone, Copy)]
struct Registered {
    id: i32,
    modifiers: HOT_KEY_MODIFIERS,
    key: u32,
}
pub(crate) struct HotkeyRegistration {
    window: HWND,
    active: Option<Registered>,
    next_id: i32,
    first_id: i32,
    last_id: i32,
}
impl HotkeyRegistration {
    pub(crate) fn new(window: HWND) -> Self {
        Self::for_target(window, HotkeyTarget::Capture)
    }
    pub(crate) fn for_target(window: HWND, target: HotkeyTarget) -> Self {
        let (first_id, last_id) = id_range(target);
        Self::with_range(window, first_id, last_id)
    }
    pub(crate) fn for_quick_action(window: HWND, slot: usize) -> Self {
        assert!(slot < 16);
        let first = 0x8000 + slot as i32 * 0x400;
        Self::with_range(window, first, first + 0x3ff)
    }
    fn with_range(window: HWND, first_id: i32, last_id: i32) -> Self {
        Self {
            window,
            active: None,
            next_id: first_id,
            first_id,
            last_id,
        }
    }
    pub(crate) fn accepts(&self, id: usize) -> bool {
        self.active.is_some_and(|active| active.id as usize == id)
    }
    pub(crate) fn slot_contains(&self, slot: usize) -> bool {
        self.first_id == 0x8000 + slot as i32 * 0x400
    }
    pub(crate) fn binding(&self) -> Option<(HOT_KEY_MODIFIERS, u32)> {
        self.active.map(|active| (active.modifiers, active.key))
    }
    pub(crate) fn active_id(&self) -> Option<i32> {
        self.active.map(|active| active.id)
    }
    pub(crate) fn accepts_event(&self, id: usize, payload: usize) -> bool {
        self.accepts(id)
            && self.active.is_some_and(|active| {
                (payload as u32 & 0xf) == (active.modifiers.0 & 0xf)
                    && ((payload >> 16) as u32 & 0xffff) == active.key
            })
    }
    fn allocate_id(&mut self) -> i32 {
        loop {
            if self.next_id > self.last_id {
                // 仅在耗尽系统应用 ID 范围时排空本窗口旧热键消息，然后才复用 ID。
                unsafe {
                    use windows::Win32::UI::WindowsAndMessaging::*;
                    let mut message = MSG::default();
                    while PeekMessageW(
                        &mut message,
                        Some(self.window),
                        WM_HOTKEY,
                        WM_HOTKEY,
                        PM_REMOVE,
                    )
                    .as_bool()
                    {}
                }
                self.next_id = self.first_id;
            }
            let id = self.next_id;
            self.next_id += 1;
            if self.active.is_none_or(|active| active.id != id) {
                return id;
            }
        }
    }
    pub(crate) fn set(&mut self, requested: Option<(HOT_KEY_MODIFIERS, u32)>) -> Result<bool> {
        if self.binding() == requested {
            return Ok(false);
        }
        let Some((modifiers, key)) = requested else {
            if let Some(active) = self.active {
                unsafe {
                    UnregisterHotKey(Some(self.window), active.id)?;
                }
            }
            self.active = None;
            return Ok(true);
        };
        let id = self.allocate_id();
        unsafe {
            RegisterHotKey(Some(self.window), id, modifiers, key)?;
        }
        if let Some(old) = self.active {
            if let Err(error) = unsafe { UnregisterHotKey(Some(self.window), old.id) } {
                let _ = unsafe { UnregisterHotKey(Some(self.window), id) };
                return Err(error.into());
            }
        }
        self.active = Some(Registered { id, modifiers, key });
        Ok(true)
    }
}
fn id_range(target: HotkeyTarget) -> (i32, i32) {
    match target {
        HotkeyTarget::Capture => (1, 0x5fff),
        HotkeyTarget::ClipboardPin => (0x6000, 0x7fff),
    }
}
impl Drop for HotkeyRegistration {
    fn drop(&mut self) {
        let _ = self.set(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_id_ranges_are_disjoint_and_use_only_application_ids() {
        let capture = id_range(HotkeyTarget::Capture);
        let pin = id_range(HotkeyTarget::ClipboardPin);
        assert!(capture.0 > 0 && capture.0 <= capture.1);
        assert!(pin.0 <= pin.1 && pin.1 <= 0xbfff);
        assert!(capture.1 < pin.0);
        let capture = HotkeyRegistration::new(HWND::default());
        let pin = HotkeyRegistration::for_target(HWND::default(), HotkeyTarget::ClipboardPin);
        assert_ne!(capture.next_id, pin.next_id);
        // 构造空注册器不调用 Win32，也不会在 Drop 注册或注销真实组合。
        assert!(capture.active.is_none() && pin.active.is_none());
    }
}
