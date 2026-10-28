//! DWM 负责实时合成；本模块从不读取桌面或源窗口像素。
use crate::{
    geometry::ScreenRect,
    pin_live_model::{SourceIdentity, SourceState},
    Result,
};
use windows::{
    core::HRESULT,
    Win32::{Foundation::*, Graphics::Gdi::ClientToScreen, UI::WindowsAndMessaging::*},
};

#[repr(C)]
struct ThumbnailProperties {
    flags: u32,
    destination: RECT,
    source: RECT,
    opacity: u8,
    visible: i32,
    client_only: i32,
}

#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmRegisterThumbnail(destination: HWND, source: HWND, thumbnail: *mut isize) -> HRESULT;
    fn DwmUnregisterThumbnail(thumbnail: isize) -> HRESULT;
    fn DwmQueryThumbnailSourceSize(thumbnail: isize, size: *mut SIZE) -> HRESULT;
    fn DwmUpdateThumbnailProperties(
        thumbnail: isize,
        properties: *const ThumbnailProperties,
    ) -> HRESULT;
    fn DwmGetWindowAttribute(
        window: HWND,
        attribute: u32,
        value: *mut core::ffi::c_void,
        size: u32,
    ) -> HRESULT;
}

pub(crate) struct Thumbnail(isize);
impl Thumbnail {
    pub fn register(destination: HWND, source: HWND) -> Result<Self> {
        let mut handle = 0;
        unsafe {
            DwmRegisterThumbnail(destination, source, &mut handle).ok()?;
        }
        if handle == 0 {
            return Err("DWM 未返回缩略图".into());
        }
        Ok(Self(handle))
    }
    pub fn size(&self) -> Result<(u32, u32)> {
        let mut size = SIZE::default();
        unsafe {
            DwmQueryThumbnailSourceSize(self.0, &mut size).ok()?;
        }
        if size.cx <= 0 || size.cy <= 0 {
            return Err("DWM 源窗口尺寸无效".into());
        }
        Ok((size.cx as u32, size.cy as u32))
    }
    pub fn update(
        &self,
        source: ScreenRect,
        destination: ScreenRect,
        opacity: u8,
        visible: bool,
    ) -> Result<()> {
        fn native(rect: ScreenRect) -> RECT {
            RECT {
                left: rect.left,
                top: rect.top,
                right: rect.left.saturating_add_unsigned(rect.width),
                bottom: rect.top.saturating_add_unsigned(rect.height),
            }
        }
        let properties = ThumbnailProperties {
            flags: 0x1 | 0x2 | 0x4 | 0x8 | 0x10,
            destination: native(destination),
            source: native(source),
            opacity,
            visible: i32::from(visible),
            client_only: 0,
        };
        unsafe {
            DwmUpdateThumbnailProperties(self.0, &properties).ok()?;
        }
        Ok(())
    }
    pub fn hide(&self) -> Result<()> {
        let properties = ThumbnailProperties {
            flags: 0x8,
            destination: RECT::default(),
            source: RECT::default(),
            opacity: 255,
            visible: 0,
            client_only: 0,
        };
        unsafe {
            DwmUpdateThumbnailProperties(self.0, &properties).ok()?;
        }
        Ok(())
    }
}
impl Drop for Thumbnail {
    fn drop(&mut self) {
        unsafe {
            let _ = DwmUnregisterThumbnail(self.0);
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SourceProbe {
    pub state: SourceState,
    pub window: ScreenRect,
    pub extended: ScreenRect,
    pub client: ScreenRect,
}

pub(crate) fn probe(identity: SourceIdentity) -> SourceProbe {
    let empty = ScreenRect {
        left: 0,
        top: 0,
        width: 0,
        height: 0,
    };
    let mut result = SourceProbe {
        state: SourceState::Lost,
        window: empty,
        extended: empty,
        client: empty,
    };
    let hwnd = HWND(identity.hwnd as *mut _);
    unsafe {
        let mut pid = 0;
        let tid = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if !IsWindow(Some(hwnd)).as_bool()
            || pid != identity.process_id
            || tid != identity.thread_id
        {
            return result;
        }
        result.state = if IsIconic(hwnd).as_bool() {
            SourceState::Minimized
        } else {
            SourceState::Live
        };
        let mut cloaked = 0_u32;
        if DwmGetWindowAttribute(hwnd, 14, (&mut cloaked as *mut u32).cast(), 4).is_ok()
            && cloaked != 0
        {
            result.state = SourceState::Cloaked;
        }
        let mut window = RECT::default();
        let mut client = RECT::default();
        let mut origin = POINT::default();
        if GetWindowRect(hwnd, &mut window).is_err()
            || GetClientRect(hwnd, &mut client).is_err()
            || !ClientToScreen(hwnd, &mut origin).as_bool()
        {
            result.state = SourceState::Unavailable;
            return result;
        }
        let to_rect = |r: RECT| ScreenRect {
            left: r.left,
            top: r.top,
            width: (i64::from(r.right) - i64::from(r.left)).max(0) as u32,
            height: (i64::from(r.bottom) - i64::from(r.top)).max(0) as u32,
        };
        result.window = to_rect(window);
        result.client = ScreenRect {
            left: origin.x,
            top: origin.y,
            width: (client.right - client.left).max(0) as u32,
            height: (client.bottom - client.top).max(0) as u32,
        };
        let mut extended = window;
        let _ = DwmGetWindowAttribute(
            hwnd,
            9,
            (&mut extended as *mut RECT).cast(),
            std::mem::size_of::<RECT>() as u32,
        );
        result.extended = to_rect(extended);
        if result.client.width == 0 || result.client.height == 0 {
            result.state = SourceState::Unavailable;
        }
    }
    result
}
