//! 二维码悬浮按钮专项：生产图片编辑入口打开深色底测试图，设选区后等待自动识别，
//! 用自有窗口消息点按钮／弹层（不移动真实鼠标，不写剪贴板，不打开浏览器）。
use crate::{
    capture::Image, capture_view::CaptureView, geometry::Rect, gpui_capture::hwnd,
    gpui_smoke::Input, Result,
};
use gpui::{div, prelude::*, App, AppContext, Application, AsyncApp, Window, WindowHandle};
use serde_json::{json, Value};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

const LINK: &str = "https://example.com/rustshot";
const TEXT: &str = "RustShot 二维码文本";
/// 由 qrcode 0.14.1 按上面两段内容生成的模块矩阵（# 为深色模块）。
const LINK_MODULES: [&str; 29] = [
    "#######....#.#..#..#..#######",
    "#.....#.#..##.###..##.#.....#",
    "#.###.#....####.#.....#.###.#",
    "#.###.#..#..####..###.#.###.#",
    "#.###.#.#.#.#.#....#..#.###.#",
    "#.....#...####...###..#.....#",
    "#######.#.#.#.#.#.#.#.#######",
    ".........#...##.##...........",
    "#.#.#.#...#.#.##.#......#..#.",
    ".##.##...##...##.##.#.#..#..#",
    ".##...#..##.##...##...###.###",
    "##.##...#.#.#..#.##.##..#..#.",
    ".######..####...##.####..#.##",
    "..####...#..##.##....##..#..#",
    "#..####.##.#..###.#..#.###.##",
    "####.....##..##.##.###.#.#.#.",
    "##..#.#.####..##.#.####..#.##",
    "...###..#####.##....###..##.#",
    "#.###.#.##.#.#...#...##.#..##",
    ".##.##.##..##..#.##.#..###.#.",
    "#.######........##.######....",
    "........##...#.######...#.###",
    "#######...###.###..##.#.##.##",
    "#.....#....####.##..#...##.#.",
    "#.###.#.###...##..#.#####..#.",
    "#.###.#..###.###....##..#.#..",
    "#.###.#.#...###..#..#..###..#",
    "#.....#...#.#.##...##.#....#.",
    "#######.#..#.#..#..##..##..##",
];
const TEXT_MODULES: [&str; 25] = [
    "#######.#..##..#..#######",
    "#.....#.##.#..##..#.....#",
    "#.###.#....##.#.#.#.###.#",
    "#.###.#.##..####..#.###.#",
    "#.###.#..#.##.##..#.###.#",
    "#.....#..#....##..#.....#",
    "#######.#.#.#.#.#.#######",
    "........#..######........",
    "#.##.###.###.#..#.#..#.##",
    "..##.#.#......#.#..###.#.",
    "...#.##.#.##.#.#....###..",
    "....##.#.##........#.###.",
    ".#..###.#.##...##...#.###",
    ".##....#.#.#..##..#.##..#",
    ".###.###....#.##..##..##.",
    "#....#.##...###....#.....",
    "...##.#....#.#.#######.#.",
    "........##..##.##...#.##.",
    "#######.#.#####.#.#.#.###",
    "#.....#.#...###.#...##.#.",
    "#.###.#..#.#.#.######...#",
    "#.###.#.###.###.#..######",
    "#.###.#.##.#...#.#.##..#.",
    "#.....#...#.#....##.###..",
    "#######.#..###..#.#######",
];
const WAIT: Duration = Duration::from_secs(15);
const SCALE: u32 = 5;
/// 深色底与中灰静区：避免测试窗口出现大块亮色。
const BACKGROUND: u8 = 0x26;
const QUIET: u8 = 0x6e;

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;
}

async fn past_double_click(cx: &mut AsyncApp) {
    let interval = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() };
    cx.background_executor()
        .timer(Duration::from_millis(u64::from(interval) + 80))
        .await;
}

/// 画一个码（含 4 模块静区），返回码区中心。
fn paint(image: &mut Image, modules: &[&str], left: u32, top: u32) -> (f32, f32) {
    let n = modules.len() as u32;
    let side = (n + 8) * SCALE;
    for y in top..top + side {
        for x in left..left + side {
            let i = ((y * image.width + x) * 4) as usize;
            image.bgra[i..i + 3].fill(QUIET);
        }
    }
    for (my, row) in modules.iter().enumerate() {
        for (mx, cell) in row.bytes().enumerate() {
            if cell != b'#' {
                continue;
            }
            for dy in 0..SCALE {
                for dx in 0..SCALE {
                    let x = left + (mx as u32 + 4) * SCALE + dx;
                    let y = top + (my as u32 + 4) * SCALE + dy;
                    let i = ((y * image.width + x) * 4) as usize;
                    image.bgra[i..i + 3].fill(0);
                }
            }
        }
    }
    let half = side as f32 / 2.;
    (left as f32 + half, top as f32 + half)
}

fn fixture() -> (Image, [(f32, f32); 2]) {
    let (width, height) = (900, 520);
    let mut image = Image {
        width,
        height,
        bgra: [BACKGROUND, BACKGROUND, BACKGROUND, 255].repeat((width * height) as usize),
        readback_ms: 0.,
    };
    let link = paint(&mut image, &LINK_MODULES, 80, 100);
    let text = paint(&mut image, &TEXT_MODULES, 520, 140);
    (image, [link, text])
}

async fn wait_for(
    window: WindowHandle<CaptureView>,
    cx: &mut AsyncApp,
    message: &str,
    check: impl Fn(&CaptureView) -> bool,
) -> Result<Duration> {
    let started = Instant::now();
    loop {
        if window.update(cx, |v, w, _| {
            w.refresh();
            check(v)
        })? {
            return Ok(started.elapsed());
        }
        ensure(started.elapsed() < WAIT, message)?;
        tick(cx).await;
    }
}

async fn position(
    window: WindowHandle<CaptureView>,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<(f32, f32)> {
    window.update(cx, |v, _, _| v.ui_positions.borrow_mut().remove(id))?;
    let mut found = None;
    wait_for(window, cx, "等待控件实际绘制超时", |v| {
        v.ui_positions.borrow().contains_key(id)
    })
    .await?;
    window.update(cx, |v, _, _| found = v.ui_positions.borrow().get(id).copied())?;
    found.ok_or_else(|| format!("找不到控件：{id}").into())
}

async fn steps(window: WindowHandle<CaptureView>, centers: [(f32, f32); 2], cx: &mut AsyncApp) -> Result<Value> {
    let input = window.update(cx, |_, w, _| -> Result<Input> {
        crate::gpui_smoke::isolate_input(hwnd(w))?;
        Ok(Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
    })??;
    // 选区覆盖两个码；之后的识别、按钮和弹层全部走生产路径。
    let crop = Rect {
        x: 40,
        y: 60,
        width: 820,
        height: 420,
    };
    wait_for(window, cx, "图片编辑器没有就绪", |v| !v.processing && !v.saving).await?;
    window.update(cx, |v, _, cx| {
        v.editor.set_crop(Some(crop));
        cx.notify();
    })?;
    let detect = wait_for(window, cx, "选区稳定后没有自动识别到两个二维码", |v| {
        v.qr.marks.len() == 2
    })
    .await?;
    let marks = window.update(cx, |v, _, _| v.qr.marks.clone())?;
    let link_index = marks
        .iter()
        .position(|m| m.text == LINK)
        .ok_or("缺少链接二维码")?;
    let text_index = marks
        .iter()
        .position(|m| m.text == TEXT)
        .ok_or("缺少文本二维码")?;
    for (mark, expected) in [(&marks[link_index], centers[0]), (&marks[text_index], centers[1])] {
        ensure(
            (mark.center.0 - expected.0).abs() < 4. && (mark.center.1 - expected.1).abs() < 4.,
            "识别出的二维码中心与测试图不符",
        )?;
    }
    // 按钮实际绘制位置 = 视口映射后的码中心。
    let expected_button = window.update(cx, |v, w, _| {
        let scale = v.canvas_scale(w);
        let offset = v.canvas_offset(w);
        let c = v.qr.marks[0].center;
        (
            f32::from(offset.x) + c.0 / scale,
            f32::from(offset.y) + c.1 / scale,
        )
    })?;
    let button = position(window, "qr-button", cx).await?;
    ensure(
        (button.0 - expected_button.0).abs() < 1.5 && (button.1 - expected_button.1).abs() < 1.5,
        "悬浮按钮没有画在二维码中心",
    )?;

    // 点第一个按钮展开弹层；再点一次收起；Esc 只收起弹层不退出。
    input.click(button);
    wait_for(window, cx, "点击按钮没有展开弹层", |v| v.qr.open == Some(0)).await?;
    let copy = position(window, "qr-copy", cx).await?;
    let show = position(window, "qr-show-text", cx).await?;
    let has_open = window.update(cx, |v, _, _| v.qr.marks[0].text == LINK)?;
    let open = if has_open {
        Some(position(window, "qr-open", cx).await?)
    } else {
        None
    };
    let popup_png = save_window(window, cx).await.unwrap_or_else(|e| format!("截图失败：{e}"));
    // 两次单击间隔超过系统双击时间，否则链接码会被当成双击直接打开浏览器。
    past_double_click(cx).await;
    input.click(button);
    wait_for(window, cx, "再次点击按钮没有收起弹层", |v| v.qr.open.is_none()).await?;
    past_double_click(cx).await;
    input.click(button);
    wait_for(window, cx, "第三次点击没有重新展开", |v| v.qr.open == Some(0)).await?;
    input.key(0x1b);
    wait_for(window, cx, "Esc 没有收起弹层", |v| v.qr.open.is_none()).await?;
    tick(cx).await;
    ensure(
        window.update(cx, |v, _, _| v.qr.marks.len() == 2).is_ok(),
        "Esc 关闭了图片编辑器",
    )?;

    // 悬停即展开（不固定），移开后延时收起。
    let physical = |p: (f32, f32)| {
        (
            (p.0 * input.scale).round() as u32,
            (p.1 * input.scale).round() as u32,
        )
    };
    input.hover(physical((button.0 - 60., button.1 - 60.)))?;
    tick(cx).await;
    input.hover(physical(button))?;
    wait_for(window, cx, "悬停按钮没有展开弹层", |v| v.qr.open == Some(0)).await?;
    input.hover(physical((button.0 - 60., button.1 - 60.)))?;
    wait_for(window, cx, "移开后弹层没有自动收起", |v| v.qr.open.is_none()).await?;

    // 文本码：展开后在结果面板中查看，按钮随结果面板隐藏。
    let text_button = window.update(cx, |v, w, _| {
        let scale = v.canvas_scale(w);
        let offset = v.canvas_offset(w);
        let c = v.qr.marks[text_index].center;
        (
            f32::from(offset.x) + c.0 / scale,
            f32::from(offset.y) + c.1 / scale,
        )
    })?;
    input.click(text_button);
    wait_for(window, cx, "文本二维码按钮没有展开弹层", |v| {
        v.qr.open == Some(text_index)
    })
    .await?;
    let show_text = position(window, "qr-show-text", cx).await?;
    input.click(show_text);
    wait_for(window, cx, "查看全文没有进入结果面板", |v| {
        v.recognition_result.as_deref() == Some(TEXT) && v.qr.open.is_none()
    })
    .await?;

    // 选区移到没有码的地方：旧结果立即作废，稳定后也不再出现。
    window.update(cx, |v, _, cx| {
        v.recognition_result = None;
        v.editor.set_crop(Some(Rect {
            x: 360,
            y: 20,
            width: 120,
            height: 120,
        }));
        cx.notify();
    })?;
    wait_for(window, cx, "选区变化后旧二维码结果没有作废", |v| {
        v.qr.marks.is_empty()
    })
    .await?;
    for _ in 0..16 {
        tick(cx).await;
    }
    ensure(
        window.update(cx, |v, _, _| v.qr.marks.is_empty())?,
        "无码选区出现了二维码按钮",
    )?;
    Ok(json!({
        "detect_after_crop_ms": detect.as_secs_f64() * 1000.,
        "button": button, "expected_button": expected_button,
        "popup_rows": {"copy": copy, "open": open, "show_text": show},
        "popup_png": popup_png,
        "not_clicked": ["qr-copy (剪贴板)", "qr-open (浏览器)"],
    }))
}

/// 只读本窗口客户区所在的桌面范围，保存到 target/qa/qr-smoke。
async fn save_window(window: WindowHandle<CaptureView>, cx: &mut AsyncApp) -> Result<String> {
    use windows::Win32::{Foundation::RECT, UI::WindowsAndMessaging::GetWindowRect};
    for _ in 0..6 {
        tick(cx).await;
    }
    let handle = window.update(cx, |_, w, _| hwnd(w))?;
    let mut rect = RECT::default();
    unsafe { GetWindowRect(handle, &mut rect)? };
    let desktop = crate::desktop::DesktopCapture::capture()?;
    let bounds = desktop.bounds;
    let left = (rect.left - bounds.left).max(0) as u32;
    let top = (rect.top - bounds.top).max(0) as u32;
    let region = Rect {
        x: left,
        y: top,
        width: ((rect.right - bounds.left) as u32).min(bounds.width) - left,
        height: ((rect.bottom - bounds.top) as u32).min(bounds.height) - top,
    };
    let image = desktop.read_region(region)?;
    let path = PathBuf::from("target/qa/qr-smoke/popup.png");
    crate::export::save_png(image, &path)?;
    Ok(path.display().to_string())
}

struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
    }
}

pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let root = std::env::current_dir()?.join("target/qa/qr-smoke");
    std::fs::create_dir_all(root.join("settings"))?;
    std::env::set_var("RUSTSHOT_DATA_DIR", root.join("settings"));
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    crate::preferences::Preferences {
        history: false,
        pin_restore: false,
        ..Default::default()
    }
    .save()?;
    let (image, centers) = fixture();
    // 先确认深色低对比测试图本身可解码，失败时不打开窗口。
    let hits = crate::recognition::qr_codes(fixture().0);
    ensure(hits.len() == 2, "测试图本身无法解出两个二维码")?;
    let report = Rc::new(RefCell::new(json!({"ok": false, "error": "未完成"})));
    let final_report = report.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            let host = cx.open_window(
                gpui::WindowOptions {
                    show: false,
                    focus: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|_| Host),
            );
            let window = host
                .map_err(|e| e.to_string())
                .and_then(|_| crate::image_editor::open(image, None, cx).map_err(|e| e.to_string()));
            cx.spawn(async move |cx| {
                let result: Result<Value> = async {
                    let window = window?;
                    steps(window, centers, cx).await
                }
                .await;
                *report.borrow_mut() = match result {
                    Ok(details) => json!({"ok": true, "details": details}),
                    Err(e) => json!({"ok": false, "error": e.to_string()}),
                };
                let _ = cx.update(|cx| {
                    for w in cx.windows() {
                        let _ = cx.update_window(w, |_, w, _| w.remove_window());
                    }
                    cx.quit();
                });
            })
            .detach();
        });
    let report = final_report.borrow().clone();
    std::fs::write(
        root.join("report.json"),
        serde_json::to_string_pretty(&report)?,
    )?;
    if report["ok"] == true {
        Ok(())
    } else {
        Err(report["error"].as_str().unwrap_or("失败").to_owned().into())
    }
}
