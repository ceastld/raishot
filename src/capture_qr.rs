//! 选区稳定后在后台识别二维码，在码中心显示悬浮按钮；悬停弹出操作（复制、打开链接、
//! 查看全文），单击固定弹层，链接码双击直接打开（文本码双击只固定弹层）。
//! 只读冻结底图的选区像素（不含标注），结果以全屏坐标保存，选区变化即作废重扫。
use crate::{
    capture_toolbar::Palette,
    capture_view::{Action, CaptureView},
    geometry::Rect,
    recognition::QrHit,
    toolbar_icons::{self, Icon},
};
use gpui::{div, prelude::*, px, rgb, Context, MouseButton, Window};
use std::time::Duration;

/// 选区停止变化后再读像素，连续微调时不反复读回。
const SETTLE: Duration = Duration::from_millis(140);
/// 超过约 4K 整屏的选区不自动扫描。
const MAX_PIXELS: u64 = 9_000_000;
const BUTTON: f32 = 32.;
const POPUP_WIDTH: f32 = 280.;
/// 鼠标离开按钮／弹层后稍等再收起，留出从按钮移到弹层的时间。
const HOVER_CLOSE: Duration = Duration::from_millis(280);

#[derive(Clone, Debug)]
pub(crate) struct QrMark {
    pub text: String,
    /// 全屏物理像素坐标。
    pub center: (f32, f32),
}

#[derive(Default)]
pub(crate) struct QrState {
    /// 当前已扫描或等待扫描的选区。
    target: Option<Rect>,
    generation: u64,
    pub(crate) marks: Vec<QrMark>,
    pub(crate) open: Option<usize>,
    /// 单击固定的弹层不随鼠标离开收起。
    pinned: bool,
    copied: Option<usize>,
    /// 本次按下前展开的弹层及是否固定；捕获阶段先关闭，按钮或弹层自身再据此恢复/切换。
    down_open: Option<(usize, bool)>,
    hover_button: Option<usize>,
    hover_popup: bool,
    hover_generation: u64,
    /// 正在交给系统打开链接；期间重复点击／双击不再触发。
    opening: bool,
}

impl QrState {
    pub(crate) fn is_opening(&self) -> bool {
        self.opening
    }
    pub(crate) fn invalidate_source(&mut self) {
        // 保留递增代次，不能重置成 Default 后让旧来源的回执撞上新扫描。
        self.reset(None);
    }
    fn reset(&mut self, target: Option<Rect>) {
        *self = Self {
            target,
            generation: self.generation.wrapping_add(1),
            ..Default::default()
        };
    }
}

/// 只把 http(s) 与 www. 开头且不含空白的内容当作可打开的网址。
pub(crate) fn web_url(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if let Some(rest) = ["https://", "http://"]
        .iter()
        .find_map(|prefix| lower.strip_prefix(prefix))
    {
        return (!rest.is_empty()).then(|| text.to_owned());
    }
    (lower.starts_with("www.") && lower.len() > 4 && lower[4..].contains('.'))
        .then(|| format!("https://{text}"))
}

/// 弹层位置：优先放在按钮右侧，放不下放左侧，两侧都放不下放到按钮上下方。
/// 弹层绝不能盖住按钮，否则双击的第二下会落到弹层的“复制”行上。
fn popup_origin((x, y): (f32, f32), height: f32, (view_w, view_h): (f32, f32)) -> (f32, f32) {
    const MARGIN: f32 = 8.;
    let gap = BUTTON / 2. + 6.;
    let side_top = if y + height + MARGIN <= view_h {
        y - 14.
    } else {
        y - height + 14.
    }
    .min(view_h - height - MARGIN)
    .max(MARGIN);
    if x + gap + POPUP_WIDTH + MARGIN <= view_w {
        return (x + gap, side_top);
    }
    if x - gap - POPUP_WIDTH >= MARGIN {
        return (x - gap - POPUP_WIDTH, side_top);
    }
    let left = (x - POPUP_WIDTH / 2.)
        .min(view_w - POPUP_WIDTH - MARGIN)
        .max(MARGIN);
    let top = if y + gap + height + MARGIN <= view_h {
        y + gap
    } else {
        (y - gap - height).max(MARGIN)
    };
    (left, top)
}

/// 在调用线程上用系统默认浏览器打开；只在独立线程调用（按微软要求先初始化 STA COM）。
pub(crate) fn open_in_browser(url: &str) -> std::result::Result<(), String> {
    use windows::{
        core::{w, HSTRING},
        Win32::{
            System::Com::{
                CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
            },
            UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        },
    };
    let result = unsafe {
        let com = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        let result = ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(url),
            None,
            None,
            SW_SHOWNORMAL,
        );
        if com.is_ok() {
            CoUninitialize();
        }
        result
    };
    // ShellExecuteW 失败时返回值本身就是错误码（≤ 32）。
    let code = result.0 as isize;
    if code <= 32 {
        return Err(format!("ShellExecute {code}"));
    }
    Ok(())
}

fn marks_for(hits: Vec<QrHit>, rect: Rect) -> Vec<QrMark> {
    hits.into_iter()
        .map(|hit| {
            let (x, y) = hit.center();
            QrMark {
                center: (rect.x as f32 + x, rect.y as f32 + y),
                text: hit.text,
            }
        })
        .collect()
}

impl CaptureView {
    fn qr_scan_target(&self) -> Option<Rect> {
        if self.pin_mode || self.editor.is_changing_crop() || self.manual_preview.is_some() {
            return None;
        }
        let rect = self.selected().ok()?;
        (rect.width >= 21
            && rect.height >= 21
            && u64::from(rect.width) * u64::from(rect.height) <= MAX_PIXELS)
            .then_some(rect)
    }

    /// 每次渲染核对选区；选区变化后作废旧结果，稳定一小段时间再读像素并在后台识别。
    pub(crate) fn sync_qr_scan(&mut self, cx: &mut Context<Self>) {
        let target = self.qr_scan_target();
        if target == self.qr.target {
            return;
        }
        self.qr.reset(target);
        let Some(rect) = target else {
            return;
        };
        let generation = self.qr.generation;
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(SETTLE).await;
            let image = view.update(cx, |v, _| {
                (v.qr.generation == generation)
                    .then(|| v.source.read_region(rect).ok())
                    .flatten()
            });
            let Ok(Some(image)) = image else {
                return;
            };
            let hits = cx
                .background_executor()
                .spawn(async move { crate::recognition::qr_codes(image) })
                .await;
            let _ = view.update(cx, |v, cx| {
                if v.qr.generation == generation && !hits.is_empty() {
                    v.qr.marks = marks_for(hits, rect);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn qr_buttons_visible(&self) -> bool {
        !self.qr.marks.is_empty()
            && matches!(
                self.editor.tool,
                crate::annotation::Tool::Crop | crate::annotation::Tool::Edit
            )
            && !self.processing
            && !self.saving
            && !self.editor.is_dragging()
            && !self.editor.has_pending_drawing()
            && self.editor.text_draft.is_none()
            && self.recognition_result.is_none()
            && self.ocr_overlay.is_none()
            && self.result_editor.is_none()
            && self.manual_preview.is_none()
    }

    /// 画布捕获阶段：任何按下先收起弹层，按钮和弹层自身在冒泡阶段再恢复或切换。
    pub(crate) fn qr_pointer_down_capture(&mut self, cx: &mut Context<Self>) {
        self.qr.down_open = self.qr.open.take().map(|index| (index, self.qr.pinned));
        self.qr.pinned = false;
        if self.qr.down_open.is_some() {
            cx.notify();
        }
    }

    /// 悬停进入按钮即展开该码的弹层（未固定）；离开按钮和弹层后延时收起。
    fn qr_hover(&mut self, index: Option<usize>, hovered: bool, cx: &mut Context<Self>) {
        match (index, hovered) {
            (Some(index), true) => {
                self.qr.hover_button = Some(index);
                if self.qr.open != Some(index) {
                    self.qr.open = Some(index);
                    self.qr.pinned = false;
                    self.qr.copied = None;
                    self.qr.hover_popup = false;
                }
            }
            (Some(index), false) => {
                if self.qr.hover_button == Some(index) {
                    self.qr.hover_button = None;
                }
            }
            (None, hovered) => self.qr.hover_popup = hovered,
        }
        cx.notify();
        if hovered {
            return;
        }
        self.qr.hover_generation = self.qr.hover_generation.wrapping_add(1);
        let generation = self.qr.hover_generation;
        cx.spawn(async move |view, cx| {
            cx.background_executor().timer(HOVER_CLOSE).await;
            let _ = view.update(cx, |v, cx| {
                let qr = &mut v.qr;
                if qr.hover_generation == generation
                    && !qr.pinned
                    && qr.hover_button.is_none()
                    && !qr.hover_popup
                    && qr.open.take().is_some()
                {
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 按钮按下：链接码双击直接打开；文本码双击只固定弹层，不做隐式复制，
    /// 免得双击在两种码上一个打开、一个复制。单击在固定与收起之间切换。
    fn qr_button_down(
        &mut self,
        index: usize,
        click_count: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.invalidate_click_sequence();
        if click_count >= 2 {
            self.qr.open = Some(index);
            self.qr.pinned = true;
            if self
                .qr
                .marks
                .get(index)
                .is_some_and(|m| web_url(&m.text).is_some())
            {
                self.qr_open(index, window, cx);
            } else {
                cx.notify();
            }
            return;
        }
        self.qr.copied = None;
        if self.qr.down_open == Some((index, true)) {
            self.qr.open = None;
            self.qr.pinned = false;
            self.qr.hover_popup = false;
        } else {
            self.qr.open = Some(index);
            self.qr.pinned = true;
        }
        cx.notify();
    }

    /// 在弹层内按下：恢复捕获阶段收起的弹层并固定，免得操作途中因移出而收起。
    fn qr_restore_popup(&mut self) {
        if let Some((index, _)) = self.qr.down_open {
            self.qr.open = Some(index);
            self.qr.pinned = true;
        }
    }

    /// Esc 先收起二维码弹层，不直接退出截图。
    pub(crate) fn qr_escape(&mut self) -> bool {
        self.qr.copied = None;
        self.qr.pinned = false;
        self.qr.hover_popup = false;
        self.qr.open.take().is_some()
    }

    fn qr_copy(&mut self, index: usize, window: &Window, cx: &mut Context<Self>) {
        let Some(mark) = self.qr.marks.get(index) else {
            return;
        };
        match crate::export::copy_text(&mark.text, crate::gpui_capture::hwnd(window)) {
            Ok(()) => {
                self.qr.copied = Some(index);
                self.status = tr!("已复制二维码内容", "QR content copied").into();
            }
            Err(error) => {
                self.status = trf!("复制失败：{error}", "Copy failed: {error}");
            }
        }
        cx.notify();
    }

    fn qr_open(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(url) = self.qr.marks.get(index).and_then(|m| web_url(&m.text)) else {
            return;
        };
        // 不能在按键回调里同步调用 ShellExecuteW：它会跑内嵌消息循环（COM／DDE），
        // 重入仍在处理本次事件的 GPUI 窗口过程，借用冲突在 extern 回调里直接中止进程。
        // 也不能用 GPUI open_url 后立刻结束截图：后台任务还没执行进程就退出了。
        // 所以交给独立线程打开，UI 照常处理消息，成功后再结束截图（置顶全屏层会盖住浏览器）。
        if self.qr.opening {
            return;
        }
        self.qr.opening = true;
        // 前台授权按进程判断，在当前持有前台的 UI 线程上放开。
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(
                windows::Win32::UI::WindowsAndMessaging::ASFW_ANY,
            );
        }
        let task = cx.background_executor().spawn(async move {
            std::thread::Builder::new()
                .name("qr-open-url".into())
                .spawn(move || open_in_browser(&url))
                .map_err(|e| e.to_string())?
                .join()
                .map_err(|_| "ShellExecute panicked".to_owned())?
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |v, window, cx| {
                v.qr.opening = false;
                match result {
                    Ok(()) => v.perform(Action::Cancel, window, cx),
                    Err(error) => {
                        v.status = trf!("打开链接失败：{error}", "Failed to open link: {error}");
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn qr_show_text(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(mark) = self.qr.marks.get(index) else {
            return;
        };
        let text = mark.text.clone();
        self.qr.open = None;
        self.qr.pinned = false;
        self.qr.hover_popup = false;
        self.show_qr_text_result(text);
        cx.notify();
    }

    /// 按钮和弹层位于窗口坐标层；按钮放在工具栏之下，弹层放在最上层。
    pub(crate) fn render_qr_overlay(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> (Vec<gpui::AnyElement>, Option<gpui::AnyElement>) {
        if !self.qr_buttons_visible() {
            return (Vec::new(), None);
        }
        let p = Palette::new(self.light_theme);
        let scale = self.canvas_scale(window);
        let offset = self.canvas_offset(window);
        let view = window.viewport_size();
        let (view_w, view_h) = (f32::from(view.width), f32::from(view.height));
        let at = |mark: &QrMark| {
            (
                f32::from(offset.x) + mark.center.0 / scale,
                f32::from(offset.y) + mark.center.1 / scale,
            )
        };
        let mut elements = Vec::new();
        for (index, mark) in self.qr.marks.iter().enumerate() {
            let (x, y) = at(mark);
            if x < 0. || y < 0. || x > view_w || y > view_h {
                continue;
            }
            let open = self.qr.open == Some(index);
            elements.push(
                div()
                    .id(("qr-button", index))
                    .absolute()
                    .left(px(x - BUTTON / 2.))
                    .top(px(y - BUTTON / 2.))
                    .size(px(BUTTON))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(p.accent))
                    .border_2()
                    .border_color(rgb(if open { p.on_accent } else { p.background }))
                    .shadow_lg()
                    .cursor_pointer()
                    .hover(move |s| s.border_color(rgb(p.on_accent)))
                    .child(toolbar_icons::action(Icon::Qr, p.on_accent).size(px(18.)))
                    .when(index == 0, |d| d.child(self.probe("qr-button")))
                    .on_hover(cx.listener(move |v, hovered: &bool, _, cx| {
                        v.qr_hover(Some(index), *hovered, cx)
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |v, e: &gpui::MouseDownEvent, w, cx| {
                            cx.stop_propagation();
                            v.qr_button_down(index, e.click_count, w, cx);
                        }),
                    )
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .into_any_element(),
            );
        }
        let Some((index, mark)) = self
            .qr
            .open
            .and_then(|index| Some((index, self.qr.marks.get(index)?)))
        else {
            return (elements, None);
        };
        let url = web_url(&mark.text);
        let rows = 2 + usize::from(url.is_some());
        let shown: String = mark.text.chars().take(300).collect();
        let lines = shown.lines().count().max(1) + shown.chars().count() / 24;
        let height = 44. + lines.min(4) as f32 * 18. + rows as f32 * 30.;
        let (x, y) = at(mark);
        let (left, top) = popup_origin((x, y), height, (view_w, view_h));
        let row = |id: &'static str, label: &'static str, accent: bool| {
            div()
                .id(id)
                .relative()
                .h(px(28.))
                .px_2()
                .flex()
                .items_center()
                .rounded_md()
                .cursor_pointer()
                .text_color(rgb(if accent { p.accent } else { p.foreground }))
                .hover(move |s| s.bg(rgb(p.hover)))
                .child(label)
                .child(self.probe(id))
        };
        let copied = self.qr.copied == Some(index);
        let mut popup = div()
            .id("qr-popup")
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(POPUP_WIDTH))
            .p(px(6.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .rounded_lg()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .shadow_lg()
            .text_color(rgb(p.foreground))
            .text_size(px(12.))
            .cursor(gpui::CursorStyle::Arrow)
            .on_hover(cx.listener(|v, hovered: &bool, _, cx| v.qr_hover(None, *hovered, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, _, _, cx| {
                    v.invalidate_click_sequence();
                    v.qr_restore_popup();
                    cx.stop_propagation();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|v, _, _, cx| {
                    v.qr_restore_popup();
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(if url.is_some() {
                        tr!("二维码 · 链接", "QR code · link")
                    } else {
                        tr!("二维码 · 文本", "QR code · text")
                    }),
            )
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .line_clamp(4)
                    .text_ellipsis()
                    .child(shown),
            )
            .child(div().mx_1().h(px(1.)).bg(rgb(p.border)))
            .child(
                row(
                    "qr-copy",
                    if copied {
                        tr!("已复制 ✓", "Copied ✓")
                    } else if url.is_some() {
                        tr!("复制链接", "Copy link")
                    } else {
                        tr!("复制内容", "Copy text")
                    },
                    copied,
                )
                .on_click(cx.listener(move |v, _, w, cx| {
                    v.qr_copy(index, w, cx);
                    cx.stop_propagation();
                })),
            );
        if url.is_some() {
            popup = popup.child(
                row("qr-open", tr!("在浏览器中打开", "Open in browser"), false).on_click(
                    cx.listener(move |v, _, w, cx| {
                        v.qr_open(index, w, cx);
                        cx.stop_propagation();
                    }),
                ),
            );
        }
        popup = popup.child(
            row(
                "qr-show-text",
                tr!("在结果面板中查看", "Show in result panel"),
                false,
            )
            .on_click(cx.listener(move |v, _, _, cx| {
                v.qr_show_text(index, cx);
                cx.stop_propagation();
            })),
        );
        (elements, Some(popup.into_any_element()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refresh_invalidates_qr_generation_without_reusing_an_old_receipt() {
        let mut state = QrState::default();
        state.reset(Some(Rect::full(100, 100)));
        let old_generation = state.generation;
        state.marks.push(QrMark {
            text: "old".into(),
            center: (10., 10.),
        });
        state.invalidate_source();
        state.reset(Some(Rect::full(100, 100)));
        assert_ne!(state.generation, old_generation);
        assert!(state.marks.is_empty());
    }
    #[test]
    fn only_plain_web_addresses_open() {
        assert_eq!(
            web_url(" https://example.com/a?b=1 ").as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(
            web_url("HTTP://EXAMPLE.COM").as_deref(),
            Some("HTTP://EXAMPLE.COM")
        );
        assert_eq!(
            web_url("www.example.com").as_deref(),
            Some("https://www.example.com")
        );
        for text in [
            "",
            "https://",
            "hello world",
            "https://a.com/x y",
            "javascript:alert(1)",
            "file:///C:/Windows",
            "mailto:a@b.com",
            "www.",
            "WIFI:S:home;T:WPA;P:secret;;",
        ] {
            assert_eq!(web_url(text), None, "{text}");
        }
    }
    #[test]
    fn popup_never_covers_its_button() {
        let height = 150.;
        for view in [(1280., 720.), (1920., 1080.), (420., 600.), (300., 400.)] {
            let mut x = 0.;
            while x <= view.0 {
                for y in [0., 40., view.1 / 2., view.1 - 40., view.1] {
                    let (left, top) = popup_origin((x, y), height, view);
                    let half = BUTTON / 2.;
                    let overlaps = left < x + half
                        && left + POPUP_WIDTH > x - half
                        && top < y + half
                        && top + height > y - half;
                    assert!(
                        !overlaps,
                        "view {view:?} button ({x}, {y}) popup ({left}, {top})"
                    );
                }
                x += 10.;
            }
        }
        // 右侧放得下时仍在右侧。
        assert_eq!(popup_origin((100., 300.), height, (1280., 720.)).0, 122.);
    }
    #[test]
    fn marks_use_full_canvas_coordinates() {
        let hit = QrHit {
            text: "x".into(),
            corners: [(10., 10.), (30., 10.), (30., 30.), (10., 30.)],
        };
        let marks = marks_for(
            vec![hit],
            Rect {
                x: 100,
                y: 50,
                width: 40,
                height: 40,
            },
        );
        assert_eq!(marks[0].center, (120., 70.));
    }
}
