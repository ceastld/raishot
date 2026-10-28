//! 录屏工具栏的原生小浮层：按钮说明、选区尺寸标签与消息条。
//!
//! 浮层是工具栏拥有的不激活分层小窗，排除采集，出现和消失都不改变工具栏窗口尺寸。
//! 说明与尺寸标签鼠标穿透；消息条可以点击，点一下即自行隐藏，宿主轮询时发现并清除消息。
use crate::{capture_toolbar::Palette, gdi_surface::Surface, Result};
use std::sync::OnceLock;
use windows::{
    core::{w, PCWSTR},
    Win32::{
        Foundation::*, Graphics::Gdi::*, System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
};

const CLASS: PCWSTR = w!("RustShot.Recording.Bubble.v1");
const FONT_DIP: f32 = 12.;
const PAD_X_DIP: f32 = 8.;
const PAD_Y_DIP: f32 = 5.;
const GAP_DIP: f32 = 6.;
const ICON_DIP: f32 = 14.;
const RADIUS_DIP: f64 = 6.;
/// 说明气泡最宽多少（DIP）；路径之类的长文字按路径省略。
const TIP_MAX_DIP: f32 = 420.;
/// 警告色：浅色主题用深琥珀，深色主题用亮琥珀，保证与底色对比。
const WARN_LIGHT: u32 = 0x9a6200;
const WARN_DARK: u32 = 0xf2b441;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    /// 按钮说明、尺寸标签：名称正常色，“ · ”之后或第二行次要色。
    Plain,
    /// 进度类消息：无图标。
    Info,
    Warning,
    /// 错误消息右侧带 ×，点击消息条即收起。
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Look {
    pub text: String,
    pub tone: Tone,
    pub mono: bool,
    /// 消息条换行宽度（物理像素）；说明气泡不换行。
    pub max_width: i32,
    pub scale: f32,
    pub light: bool,
}

fn blend(a: u32, b: u32, t: f32) -> u32 {
    let mix = |shift: u32| {
        let (x, y) = (((a >> shift) & 0xff) as f32, ((b >> shift) & 0xff) as f32);
        ((x + (y - x) * t).round() as u32) << shift
    };
    mix(16) | mix(8) | mix(0)
}
/// 量好版面后建一次性离屏位图，画出说明、尺寸标签或消息条。
fn render(look: &Look) -> Result<Surface> {
    let scale = look.scale.max(0.5);
    let p = Palette::new(look.light);
    let px = |dip: f32| (dip * scale).round() as i32;
    let face = if look.mono {
        w!("Consolas")
    } else {
        w!("Microsoft YaHei UI")
    };
    let mut s = Surface::new(px(FONT_DIP), face)?;
    let (pad_x, pad_y, gap) = (px(PAD_X_DIP), px(PAD_Y_DIP), px(GAP_DIP));
    let tone = match look.tone {
        Tone::Warning => Some(if look.light { WARN_LIGHT } else { WARN_DARK }),
        Tone::Error => Some(p.danger),
        Tone::Plain | Tone::Info => None,
    };
    let single = DT_SINGLELINE;
    let (_, line_h) = s.measure("Ag", single, 0);
    // 先量出版面，再建位图。
    enum Layout {
        Line {
            head: (String, i32),
            rest: Option<(String, i32)>,
        },
        TwoLines {
            first: String,
            second: String,
        },
        Message {
            text_w: i32,
            text_h: i32,
            close_w: i32,
        },
    }
    let tip_max = px(TIP_MAX_DIP) - pad_x * 2;
    let (layout, width, height) = if look.tone == Tone::Plain {
        if let Some((first, second)) = look.text.split_once('\n') {
            let w1 = s.measure(first, single, 0).0.min(tip_max);
            let w2 = s.measure(second, single, 0).0.min(tip_max);
            let width = w1.max(w2) + pad_x * 2;
            let height = line_h * 2 + px(1.) + pad_y * 2;
            (
                Layout::TwoLines {
                    first: first.into(),
                    second: second.into(),
                },
                width,
                height,
            )
        } else {
            let (head, rest) = match look.text.split_once(" · ") {
                Some((head, rest)) => (head, Some(rest)),
                None => (look.text.as_str(), None),
            };
            let head_w = s.measure(head, single, 0).0.min(tip_max);
            let rest = rest.map(|rest| {
                let w = s
                    .measure(rest, single, 0)
                    .0
                    .min(tip_max - head_w - gap)
                    .max(0);
                (rest.to_owned(), w)
            });
            let width = head_w + rest.as_ref().map_or(0, |(_, w)| gap + w) + pad_x * 2;
            (
                Layout::Line {
                    head: (head.into(), head_w),
                    rest,
                },
                width,
                line_h + pad_y * 2,
            )
        }
    } else {
        let icon = if tone.is_some() {
            px(ICON_DIP) + gap
        } else {
            0
        };
        let close_w = if look.tone == Tone::Error {
            s.measure("×", single, 0).0 + gap
        } else {
            0
        };
        let limit = (look.max_width - pad_x * 2 - icon - close_w).max(px(80.));
        let (text_w, text_h) = s.measure(&look.text, DT_WORDBREAK, limit);
        let width = pad_x * 2 + icon + text_w.min(limit) + close_w;
        let height = pad_y * 2 + text_h.max(line_h);
        (
            Layout::Message {
                text_w: text_w.min(limit),
                text_h,
                close_w,
            },
            width,
            height,
        )
    };
    let (width, height) = (width.max(1) as u32, height.max(1) as u32);
    let background = tone.map_or(p.background, |c| blend(p.background, c, 0.12));
    s.allocate(width, height, background)?;
    // 警告／错误图标：实心圆（抗锯齿），稍后在上面写“!”。
    let icon_box = tone.map(|color| {
        let d = px(ICON_DIP);
        let (cx, cy) = ((pad_x + d / 2) as f32, (pad_y + line_h / 2) as f32);
        let r = d as f32 / 2.;
        let (w, h) = (width as i32, height as i32);
        let rgb = [
            (color & 0xff) as f32,
            ((color >> 8) & 0xff) as f32,
            ((color >> 16) & 0xff) as f32,
        ];
        let pixels = s.pixels();
        for y in (cy - r - 1.).floor().max(0.) as i32..((cy + r + 1.).ceil() as i32).min(h) {
            for x in (cx - r - 1.).floor().max(0.) as i32..((cx + r + 1.).ceil() as i32).min(w) {
                let dist = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                let cover = (r - dist + 0.5).clamp(0., 1.);
                if cover > 0. {
                    let i = ((y * w + x) * 4) as usize;
                    for c in 0..3 {
                        let old = f32::from(pixels[i + c]);
                        pixels[i + c] = (old + (rgb[c] - old) * cover).round() as u8;
                    }
                }
            }
        }
        RECT {
            left: pad_x,
            top: pad_y + line_h / 2 - d / 2,
            right: pad_x + d,
            bottom: pad_y + line_h / 2 - d / 2 + d,
        }
    });
    let row = |left: i32, top: i32, right: i32| RECT {
        left,
        top,
        right,
        bottom: top + line_h,
    };
    match &layout {
        Layout::Line { head, rest } => {
            s.draw(
                &head.0,
                p.foreground,
                row(pad_x, pad_y, pad_x + head.1),
                single | DT_END_ELLIPSIS,
            );
            if let Some((rest, w)) = rest {
                let left = pad_x + head.1 + gap;
                s.draw(
                    rest,
                    p.muted,
                    row(left, pad_y, left + w),
                    single | DT_END_ELLIPSIS,
                );
            }
        }
        Layout::TwoLines { first, second } => {
            let right = width as i32 - pad_x;
            s.draw(
                first,
                p.foreground,
                row(pad_x, pad_y, right),
                single | DT_END_ELLIPSIS,
            );
            let top = pad_y + line_h + px(1.);
            s.draw(
                second,
                p.muted,
                row(pad_x, top, right),
                single | DT_PATH_ELLIPSIS,
            );
        }
        Layout::Message {
            text_w,
            text_h,
            close_w,
        } => {
            let mut left = pad_x;
            if let Some(icon) = icon_box {
                s.draw("!", background, icon, single | DT_CENTER | DT_VCENTER);
                left = icon.right + gap;
            }
            s.draw(
                &look.text,
                p.foreground,
                RECT {
                    left,
                    top: pad_y,
                    right: left + text_w,
                    bottom: pad_y + text_h,
                },
                DT_WORDBREAK,
            );
            if *close_w > 0 {
                let right = width as i32 - pad_x;
                s.draw(
                    "×",
                    p.muted,
                    row(right - close_w + gap, pad_y, right),
                    single | DT_RIGHT,
                );
            }
        }
    }
    s.finish(scale, RADIUS_DIP, p.border);
    Ok(s)
}

fn register_class() -> Result<()> {
    static REGISTERED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    REGISTERED
        .get_or_init(|| {
            let instance = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
            let class = WNDCLASSW {
                hInstance: HINSTANCE(instance.0),
                lpszClassName: CLASS,
                lpfnWndProc: Some(bubble_proc),
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
unsafe extern "system" fn bubble_proc(h: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let passthrough =
        || unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } & WS_EX_TRANSPARENT.0 as isize != 0;
    match message {
        WM_NCHITTEST if passthrough() => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_SETCURSOR if !passthrough() => unsafe {
            if let Ok(cursor) = LoadCursorW(None, IDC_HAND) {
                SetCursor(Some(cursor));
            }
            LRESULT(1)
        },
        // 消息条点一下即收起；宿主轮询发现窗口已隐藏后清除消息。
        WM_LBUTTONUP => unsafe {
            let _ = ShowWindow(h, SW_HIDE);
            LRESULT(0)
        },
        _ => unsafe { DefWindowProcW(h, message, wp, lp) },
    }
}

pub(super) struct Bubble {
    owner: HWND,
    hwnd: HWND,
    look: Option<Look>,
    size: (u32, u32),
    origin: (i32, i32),
    shown: bool,
}
impl Bubble {
    pub(super) fn new(owner: HWND, clickable: bool) -> Result<Self> {
        register_class()?;
        let mut style = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST;
        if !clickable {
            style |= WS_EX_TRANSPARENT;
        }
        let hwnd = unsafe {
            CreateWindowExW(
                style,
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
        let bubble = Self {
            owner,
            hwnd,
            look: None,
            size: (0, 0),
            origin: (0, 0),
            shown: false,
        };
        // 与工具栏一样不进入录像和截图。
        crate::remote_display::exclude_from_capture(hwnd)?;
        Ok(bubble)
    }
    fn valid(&self) -> bool {
        unsafe {
            IsWindow(Some(self.hwnd)).as_bool()
                && GetWindow(self.hwnd, GW_OWNER).is_ok_and(|owner| owner == self.owner)
        }
    }
    /// 显示或更新；内容不变时只移动。place 根据气泡尺寸给出左上角（物理像素）。
    pub(super) fn show(
        &mut self,
        look: &Look,
        place: impl FnOnce((u32, u32)) -> (i32, i32),
    ) -> Result<()> {
        if !self.valid() {
            return Err("录屏提示窗口已失效".into());
        }
        if self.shown && self.look.as_ref() == Some(look) {
            let origin = place(self.size);
            if origin != self.origin {
                self.origin = origin;
                unsafe {
                    SetWindowPos(
                        self.hwnd,
                        None,
                        origin.0,
                        origin.1,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    )?;
                }
            }
            return Ok(());
        }
        let surface = render(look)?;
        let origin = place(surface.size);
        unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&POINT {
                    x: origin.0,
                    y: origin.1,
                }),
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
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                }),
                ULW_ALPHA,
            )?;
            if !self.shown {
                // 录制绘图层也是置顶窗口；每次出现都排到它上面。
                SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )?;
            }
        }
        self.look = Some(look.clone());
        self.size = surface.size;
        self.origin = origin;
        self.shown = true;
        Ok(())
    }
    /// 当前显示的尺寸；说明气泡堆叠在消息条外侧时用。
    pub(super) fn size(&self) -> Option<(u32, u32)> {
        self.shown.then_some(self.size)
    }
    pub(super) fn hide(&mut self) {
        if self.shown && self.valid() {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
        self.shown = false;
    }
    /// 用户点过消息条：窗口已自行隐藏。
    pub(super) fn dismissed(&mut self) -> bool {
        if self.shown && !unsafe { IsWindowVisible(self.hwnd) }.as_bool() {
            self.shown = false;
            return true;
        }
        false
    }
    pub(super) fn hovered(&self) -> bool {
        if !self.shown {
            return false;
        }
        let mut cursor = POINT::default();
        unsafe { GetCursorPos(&mut cursor) }.is_ok()
            && cursor.x >= self.origin.0
            && cursor.y >= self.origin.1
            && cursor.x < self.origin.0 + self.size.0 as i32
            && cursor.y < self.origin.1 + self.size.1 as i32
    }
}
impl Drop for Bubble {
    fn drop(&mut self) {
        // 工具栏先销毁时浮层已随之销毁；句柄可能被复用，只回收仍归属原工具栏的窗口。
        if self.valid() {
            unsafe {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}
