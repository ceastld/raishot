//! 本轮贴图细节验收：只操作自有合成窗口、隔离配置与本地 QA 目录。
use crate::{
    annotation::{Style, Tool},
    capture::Image,
    capture_view::CaptureView,
    export,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_model::{PinState, Placement},
    pin_native as native,
    pin_view::{PinAction, PinView},
    Result,
};
use gpui::{App, AsyncApp, WindowHandle};
use std::{cell::RefCell, path::Path, rc::Rc, time::Duration};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

const FOLDER: &str = "target/qa/pin-details";
fn fixture() -> Image {
    let mut bytes = vec![255; 600 * 360 * 4];
    for y in 0..360 {
        for x in 300..600 {
            let i = (y * 600 + x) * 4;
            bytes[i..i + 3].copy_from_slice(if (x / 12 + y / 12) % 2 == 0 {
                &[60, 150, 240]
            } else {
                &[160, 90, 30]
            });
        }
    }
    Image {
        width: 600,
        height: 360,
        bgra: bytes,
        readback_ms: 0.,
    }
}
fn escape_fixture(cx: &App) -> Image {
    let background = crate::theme::palette(cx).background;
    let pixel = [
        background as u8,
        (background >> 8) as u8,
        (background >> 16) as u8,
        255,
    ];
    Image {
        width: 600,
        height: 360,
        bgra: pixel.repeat(600 * 360),
        readback_ms: 0.,
    }
}
async fn pause(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(240))
        .await;
}
async fn idle(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<()> {
    for _ in 0..60 {
        cx.background_executor()
            .timer(Duration::from_millis(100))
            .await;
        if pin
            .update(cx, |v, _, _| !v.processing)
            .map_err(|e| e.to_string())?
        {
            return Ok(());
        }
    }
    Err("贴图细节验收：图像处理超时".into())
}
fn action(pin: WindowHandle<PinView>, a: PinAction, cx: &mut AsyncApp) -> Result<()> {
    pin.update(cx, |v, w, cx| v.perform(a, w, cx))
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn inspect<T>(
    pin: WindowHandle<PinView>,
    cx: &mut AsyncApp,
    f: impl FnOnce(&CaptureView) -> T,
) -> Result<T> {
    pin.update(cx, |v, _, cx| -> Result<T> {
        let canvas = v.canvas.as_ref().ok_or("贴图内画布不存在")?;
        Ok(f(canvas.read(cx)))
    })
    .map_err(|e| e.to_string())?
}
fn pin_input(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Input> {
    pin.update(cx, |_, w, _| Input {
        handle: hwnd(w).0 as usize,
        scale: w.scale_factor(),
    })
    .map_err(|e| e.to_string())
    .map_err(Into::into)
}
fn bar_input(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Input> {
    let bar = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("贴图悬浮栏未打开")?;
    let input = bar
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    Ok(input)
}
fn bar_rect(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Placement> {
    let bar = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("贴图悬浮栏未打开")?;
    bar.update(cx, |_, w, _| native::rect(hwnd(w)))
        .map_err(|e| e.to_string())?
}
fn properties_input(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Input> {
    let bar = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("一级工具栏未打开")?;
    let properties = bar
        .update(cx, |v, _, _| v.properties)
        .map_err(|e| e.to_string())?
        .ok_or("二级属性栏未打开")?;
    let input = properties
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    Ok(input)
}
fn properties_rect(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Placement> {
    let input = properties_input(pin, cx)?;
    native::rect(HWND(input.handle as *mut _))
}
async fn button(pin: WindowHandle<PinView>, id: &'static str, cx: &mut AsyncApp) -> Result<()> {
    let bar = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("贴图悬浮栏未打开")?;
    let point = bar
        .update(cx, |v, _, _| v.positions.borrow().get(id).copied())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("主栏按钮未绘制：{id}"))?;
    bar_input(pin, cx)?.click(point);
    pause(cx).await;
    Ok(())
}
async fn property(
    pin: WindowHandle<PinView>,
    id: &'static str,
    wheel: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    let point = inspect(pin, cx, |v| v.ui_positions.borrow().get(id).copied())?
        .ok_or_else(|| format!("共享参数按钮未绘制：{id}"))?;
    let input = properties_input(pin, cx)?;
    if wheel {
        input.wheel(point, 120)?;
    } else {
        input.click(point);
    }
    pause(cx).await;
    Ok(())
}
async fn popup_button(
    pin: WindowHandle<PinView>,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let bar = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("贴图悬浮栏未打开")?;
    let popup = bar
        .update(cx, |v, _, cx| v.popup(cx))
        .map_err(|e| e.to_string())?
        .ok_or("参数弹窗未打开")?;
    let point = inspect(pin, cx, |v| v.ui_positions.borrow().get(id).copied())?
        .ok_or_else(|| format!("参数弹窗按钮未绘制：{id}"))?;
    let input = popup
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    input.click(point);
    pause(cx).await;
    Ok(())
}
async fn fresh(cx: &mut AsyncApp) -> Result<(WindowHandle<PinView>, Input)> {
    fresh_with_fixture(cx, false).await
}
async fn fresh_with_fixture(
    cx: &mut AsyncApp,
    themed: bool,
) -> Result<(WindowHandle<PinView>, Input)> {
    let work = native::work_area((100, 100));
    let pin = cx
        .update(|cx| {
            PinView::open_state(
                if themed { escape_fixture(cx) } else { fixture() },
                (work.x + 100, work.y + 100),
                PinState::default(),
                cx,
            )
        })
        .map_err(|e| e.to_string())??;
    let input = pin_input(pin, cx)?;
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    pause(cx).await;
    input.key(0x20);
    pause(cx).await;
    Ok((pin, input))
}
async fn escape_regression(cx: &mut AsyncApp) -> Result<()> {
    for (expanded, toolbar_focus, label) in [
        (true, false, "expanded_pin"),
        (true, true, "expanded_toolbar"),
        (false, false, "collapsed_pin"),
    ] {
        let (pin, input) = fresh_with_fixture(cx, true).await?;
        let bar = bar_input(pin, cx)?;
        if !expanded {
            input.key(0x20);
            pause(cx).await;
            if pin.update(cx, |v, _, _| v.chrome || v.toolbar.is_some())
                .map_err(|e| e.to_string())?
            {
                return Err("Esc 验收未收起工具栏".into());
            }
        } else if !toolbar_focus {
            bar.key(0x31);
            pause(cx).await;
            held_mouse(input, (40, 50), true)?;
            held_mouse(input, (240, 180), false)?;
            pause(cx).await;
            if !inspect(pin, cx, |v| v.editor.is_dragging())? {
                return Err("Esc 专项没有进入真实矩形拖动".into());
            }
            input.key(0x1b);
            pause(cx).await;
            if !pin.update(cx, |v, _, _| v.chrome && v.toolbar.is_some())
                .map_err(|e| e.to_string())?
                || inspect(pin, cx, |v| {
                    v.editor.is_dragging() || !v.editor.document.items().is_empty()
                })?
            {
                return Err("Esc 专项首次按键没有只取消手势".into());
            }
            release_mouse(input, (240, 180))?;
            pause(cx).await;
            println!("{{\"test\":\"native_pin_escape_cancels_gesture_before_close\",\"ok\":true}}");
        }
        if !toolbar_focus {
            pin.update(cx, |v, w, _| w.focus(&v.focus))
                .map_err(|e| e.to_string())?;
        }
        let target = if toolbar_focus { bar } else { input };
        target.key(0x1b);
        pause(cx).await;
        if pin.update(cx, |_, _, _| ()).is_ok()
            || unsafe { IsWindow(Some(HWND(input.handle as *mut _))).as_bool() }
            || unsafe { IsWindow(Some(HWND(bar.handle as *mut _))).as_bool() }
            || cx.update(|cx| cx.windows().len()).map_err(|e| e.to_string())? != 1
        {
            return Err(format!("{label} 单次 Esc 没有关闭贴图或遗留工具栏窗口").into());
        }
        println!("{{\"test\":\"native_pin_single_escape_closes_{label}\",\"ok\":true}}");
    }
    Ok(())
}
fn held_mouse(input: Input, p: (u32, u32), down: bool) -> Result<()> {
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            if down { WM_LBUTTONDOWN } else { WM_MOUSEMOVE },
            WPARAM(1 | if down { 0 } else { gpui_smoke::SMOKE_MOVE }),
            LPARAM(((p.1 << 16) | p.0) as isize),
        )?;
    }
    Ok(())
}
fn release_mouse(input: Input, p: (u32, u32)) -> Result<()> {
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            WM_LBUTTONUP,
            WPARAM(0),
            LPARAM(((p.1 << 16) | p.0) as isize),
        )?;
    }
    Ok(())
}
fn snapshot(rect: Placement, name: &str) -> Result<()> {
    let desktop = crate::desktop::DesktopCapture::capture()?;
    let own = desktop.read_region(crate::geometry::Rect {
        x: (rect.x - desktop.bounds.left) as u32,
        y: (rect.y - desktop.bounds.top) as u32,
        width: rect.width,
        height: rect.height,
    })?;
    export::save_png(own, &Path::new(FOLDER).join(name))
}
fn style(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Style> {
    inspect(pin, cx, |v| v.editor.style)
}
fn require_style(actual: Style, expected: Style, label: &str) -> Result<()> {
    if actual != expected {
        return Err(format!("{label} 修改了错误参数：{actual:?}；预期 {expected:?}").into());
    }
    Ok(())
}
pub(crate) fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    *errors.borrow_mut() = Some("贴图细节专项未完成全部步骤".into());
    let previous_foreground = unsafe { GetForegroundWindow() }.0 as usize;
    let only_escape = std::env::var("RUSTSHOT_PIN_DETAILS_ONLY_ESCAPE").as_deref() == Ok("1");
    let initial = (|| -> Result<_> {
        std::fs::create_dir_all(FOLDER)?;
        let expected = std::env::current_dir()?.join(FOLDER).join("settings");
        if crate::preferences::data_dir() != expected {
            return Err("细节专项未隔离用户配置目录".into());
        }
        std::fs::create_dir_all(&expected)?;
        // 仅重置明确属于本专项的样式夹具，不碰用户预设。
        std::fs::write(expected.join("tool-presets.json"), "{}\n")?;
        // 数字验收以默认可见工具顺序为准，只写已核验的专项隔离配置。
        crate::toolbar_config::ToolbarConfig::default().save_at(expected.join("toolbar.json"))?;
        let work = native::work_area((100, 100));
        let pin = PinView::open_state(
            if only_escape { escape_fixture(cx) } else { fixture() },
            (work.x + 100, work.y + 100),
            PinState::default(),
            cx,
        )?;
        let input = pin.update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })?;
        gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
        Ok((pin, input))
    })();
    let (keepalive, input) = match initial {
        Ok(v) => v,
        Err(e) => {
            *errors.borrow_mut() = Some(e.to_string());
            cx.quit();
            return;
        }
    };
    cx.spawn(async move |cx| {
        let result: Result<()> = async {
            pause(cx).await;
            if only_escape {
                action(keepalive, PinAction::Hide, cx)?;
                pause(cx).await;
                escape_regression(cx).await?;
                return Ok(());
            }
            let start = std::env::var("RUSTSHOT_PIN_DETAILS_FROM").ok()
                .and_then(|s|s.parse::<u8>().ok()).unwrap_or(1).clamp(1,6);
            let skip_history = std::env::var("RUSTSHOT_PIN_DETAILS_SKIP_HISTORY").as_deref()==Ok("1");
            if start <= 2 {
            input.key(0x20);
            pause(cx).await;
            let original = keepalive.update(cx, |v, _, _| (v.original_size, v.image_size, v.state.adjustments)).map_err(|e| e.to_string())?;
            // 点击栏按钮后，后续键盘消息确实发给工具栏 HWND。
            button(keepalive, "pen", cx).await?;
            for (key, tool) in [(0x31, Tool::Rectangle), (0x32, Tool::Arrow), (0x33, Tool::Pen), (0x35, Tool::Ellipse)] {
                bar_input(keepalive, cx)?.key(key);
                pause(cx).await;
                if inspect(keepalive, cx, |v| (v.editor.tool, v.pin_editing))? != (tool, true) { return Err(format!("工具栏焦点快捷键 {key:X} 没有选择 {tool:?}").into()); }
                if keepalive.update(cx, |v, _, _| (v.original_size, v.image_size, v.state.adjustments)).map_err(|e| e.to_string())? != original { return Err("绘制工具快捷键误旋转/翻转/修改原图".into()); }
            }
            println!("{{\"test\":\"native_pin_toolbar_focus_shortcuts_do_not_process_image\",\"ok\":true}}");

            bar_input(keepalive, cx)?.key(0x31);
            pause(cx).await;
            held_mouse(input, (40, 50), true)?;
            held_mouse(input, (240, 180), false)?;
            pause(cx).await;
            if !inspect(keepalive, cx, |v| v.editor.is_dragging())? { return Err("Esc 验收没有进入真实矩形拖动".into()); }
            input.key(0x1b);
            pause(cx).await;
            if !keepalive.update(cx, |v, _, _| v.chrome && v.toolbar.is_some()).map_err(|e| e.to_string())? || inspect(keepalive, cx, |v| v.editor.is_dragging() || !v.editor.document.items().is_empty())? { return Err("第一次 Esc 没有只取消拖动/错误提交矩形".into()); }
            release_mouse(input, (240, 180))?;
            input.key(0x20);
            pause(cx).await;
            if keepalive.update(cx, |v, _, _| v.chrome || v.toolbar.is_some()).map_err(|e| e.to_string())? { return Err("空格未收起工具栏".into()); }
            // 本次调整了窗口级监听的注册阶段，定向检查越界捕获与松开。
            let before = native::rect(HWND(input.handle as *mut _))?;
            input.drag((100, 120), (180, 160));
            pause(cx).await;
            let moved = native::rect(HWND(input.handle as *mut _))?;
            if (moved.x, moved.y) != (before.x + 80, before.y + 40) {
                return Err("绘制阶段注册后贴图拖动失效".into());
            }
            input.drag((599, 359), (699, 419));
            pause(cx).await;
            if native::rect(HWND(input.handle as *mut _))?.width != 700
                || keepalive.update(cx, |v, _, _| v.gesture.is_some()).map_err(|e| e.to_string())? {
                return Err("窗口边界外缩放/松开失效".into());
            }
            action(keepalive, PinAction::Hide, cx)?;
            println!("{{\"test\":\"native_pin_escape_cancels_gesture_and_space_collapses_toolbar\",\"ok\":true}}");
            } else { action(keepalive, PinAction::Hide, cx)?; }
            if start <= 2 { escape_regression(cx).await?; }

            if start <= 3 && !skip_history {
            for tool in [Tool::Text, Tool::Arrow] {
                let (pin, _) = fresh(cx).await?;
                pin.update(cx, |v, _, cx| {
                    v.canvas.as_ref().unwrap().update(cx, |v, cx| {
                        v.editor.select_tool(tool);
                        if tool == Tool::Text { v.editor.begin_text(None, crate::annotation::Point { x: 36., y: 40. }); v.editor.set_text("首个文字草稿".into()); }
                        else { v.editor.pointer_down((30, 40), 4.); v.editor.pointer_up((30, 40)); v.editor.pointer_move((160, 90)); }
                        cx.notify();
                    });
                }).map_err(|e| e.to_string())?;
                if inspect(pin, cx, |v| v.editor.document.can_undo())? { return Err("草稿验收起点已经提交".into()); }
                action(pin, PinAction::Rotate(1), cx)?;
                idle(pin, cx).await?;
                pin.update(cx, |v, _, cx| -> Result<()> {
                    if v.image_size != (360, 600) || v.pixels().bgra == v.state.adjustments.apply(&fixture())?.bgra { return Err(format!("首个 {tool:?} 草稿在旋转时丢失").into()); }
                    export::save_png(v.output(cx)?, &Path::new(FOLDER).join(if tool == Tool::Text { "first-text-rotated.png" } else { "pending-arrow-rotated.png" }))
                }).map_err(|e| e.to_string())??;
                action(pin, PinAction::Undo, cx)?;
                idle(pin, cx).await?;
                action(pin, PinAction::Undo, cx)?;
                pause(cx).await;
                if !inspect(pin, cx, |v| v.editor.document.items().is_empty() && v.editor.document.can_redo())? { return Err(format!("{tool:?} 旋转撤销未恢复可编辑草稿历史").into()); }
                action(pin, PinAction::Redo, cx)?;
                pause(cx).await;
                if !inspect(pin, cx, |v| v.editor.document.items().len() == 1 && v.editor.document.items()[0].annotation.tool == tool)? { return Err(format!("{tool:?} 草稿没有恢复为可编辑标注").into()); }
                action(pin, PinAction::Close, cx)?;
                pause(cx).await;
            }
            println!("{{\"test\":\"native_pin_first_text_and_pending_arrow_survive_processing_and_undo\",\"ok\":true}}");
            }

            if start <= 4 && !skip_history {
            let (pin, gesture_input) = fresh(cx).await?;
            button(pin, "rectangle", cx).await?;
            gesture_input.drag((40, 60), (240, 180));
            pause(cx).await;
            action(pin, PinAction::Rotate(1), cx)?;
            idle(pin, cx).await?;
            action(pin, PinAction::Undo, cx)?;
            idle(pin, cx).await?;
            action(pin, PinAction::Undo, cx)?;
            pause(cx).await;
            let boundary = pin.update(cx, |v, _, _| v.redo_edits.len()).map_err(|e| e.to_string())?;
            if boundary == 0 || !inspect(pin, cx, |v| v.editor.document.can_redo())? { return Err("边界重做验收没有建立重做状态".into()); }
            button(pin, "pen", cx).await?;
            button(pin, "rectangle", cx).await?;
            gesture_input.drag((420, 280), (420, 280));
            pause(cx).await;
            if pin.update(cx, |v, _, _| v.redo_edits.len()).map_err(|e| e.to_string())? != boundary || !inspect(pin, cx, |v| v.editor.document.can_redo())? { return Err("换工具或无变化点击清掉跨图像处理边界重做".into()); }
            action(pin, PinAction::Redo, cx)?;
            action(pin, PinAction::Redo, cx)?;
            idle(pin, cx).await?;
            if pin.update(cx, |v, _, _| (v.image_size, v.state.adjustments.turns)).map_err(|e| e.to_string())? != ((360, 600), 1) { return Err("跨标注/旋转边界的两次重做未恢复画面".into()); }
            action(pin, PinAction::Close, cx)?;
            pause(cx).await;
            println!("{{\"test\":\"native_pin_tool_switch_and_noop_click_preserve_processing_boundary_redo\",\"ok\":true}}");
            }

            if start <= 5 {
            let (pin, _) = fresh(cx).await?;
            button(pin, "rectangle", cx).await?;
            let canvas_input = pin_input(pin, cx)?;
            canvas_input.drag((40, 60), (240, 180));
            pause(cx).await;
            let original_rectangle = inspect(pin, cx, |v| v.editor.document.items().first().map(|i| i.annotation.style))?
                .ok_or_else(|| format!("属性验收没有建立矩形标注：{:?}", inspect(pin, cx, |v| (v.editor.tool, v.pin_editing, v.editor.is_dragging(), v.pointer_down_start))))?;
            let anchor = bar_rect(pin, cx)?;
            let mut expected = style(pin, cx)?;
            expected.filled = !expected.filled;
            property(pin, "filled", false, cx).await?;
            require_style(style(pin, cx)?, expected, "矩形填充")?;
            button(pin, "undo", cx).await?;
            require_style(inspect(pin, cx, |v| v.editor.document.items()[0].annotation.style)?, original_rectangle, "填充参数单步撤销")?;
            button(pin, "redo", cx).await?;
            require_style(inspect(pin, cx, |v| v.editor.document.items()[0].annotation.style)?, expected, "填充参数单步重做")?;
            canvas_input.click((100. / canvas_input.scale, 60. / canvas_input.scale));
            pause(cx).await;
            expected.radius += 1.;
            property(pin, "radius-menu", true, cx).await?;
            require_style(style(pin, cx)?, expected, "矩形圆角滚轮")?;
            property(pin, "radius-menu", false, cx).await?;
            popup_button(pin, "corner20", cx).await?;
            expected.radius = 20.;
            require_style(style(pin, cx)?, expected, "矩形圆角弹窗")?;
            if bar_rect(pin, cx)? != anchor || inspect(pin, cx, |v| v.toolbar_panel != crate::toolbar_layout::ToolbarPanel::None)? { return Err("关闭参数弹窗改变主工具栏位置/未清面板".into()); }
            snapshot(anchor, "primary-toolbar.png")?;
            snapshot(properties_rect(pin,cx)?, "rectangle-properties.png")?;
            button(pin, "text", cx).await?;
            let mut expected = style(pin, cx)?;
            expected.font_size += 1.;
            property(pin, "font-menu", true, cx).await?;
            require_style(style(pin, cx)?, expected, "文字字号滚轮")?;
            property(pin, "font-menu", false, cx).await?;
            popup_button(pin, "font40", cx).await?;
            expected.font_size = 40.;
            require_style(style(pin, cx)?, expected, "文字字号弹窗")?;
            if bar_rect(pin, cx)? != anchor { return Err("文字参数弹窗改变主栏锚点".into()); }
            snapshot(properties_rect(pin,cx)?, "text-properties.png")?;
            pin.update(cx, |v, w, cx| {
                v.canvas.as_ref().unwrap().update(cx, |v, cx| {
                    v.editor.begin_text(None, crate::annotation::Point { x: 30., y: 230. });
                    v.editor.set_text("word".into());
                    v.reset_input();
                    v.input.select_to(4, 4);
                    w.focus(&v.focus);
                    cx.notify();
                });
            }).map_err(|e| e.to_string())?;
            pause(cx).await;
            for bar in [bar_input(pin, cx)?, properties_input(pin, cx)?] {
                if unsafe { GetWindowLongPtrW(HWND(bar.handle as *mut _), GWL_EXSTYLE) }
                    & WS_EX_NOACTIVATE.0 as isize == 0 {
                    return Err("一级/二级属性栏缺少不抢输入焦点的原生样式".into());
                }
            }
            property(pin, "font-menu", false, cx).await?;
            let bar = pin.update(cx, |v, _, _| v.toolbar.unwrap()).map_err(|e| e.to_string())?;
            let popup = bar.update(cx, |v, _, cx| v.popup(cx).unwrap()).map_err(|e| e.to_string())?;
            popup.update(cx, |_, w, _| -> Result<()> { unsafe {
                // PostMessage 不模拟真实点击激活；仅本焦点用例主动激活自有参数窗。
                let current = windows::Win32::System::Threading::GetCurrentThreadId();
                let foreground_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
                let attached = foreground_thread != 0 && foreground_thread != current
                    && windows::Win32::System::Threading::AttachThreadInput(current, foreground_thread, true).as_bool();
                let _ = SetForegroundWindow(hwnd(w));
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetActiveWindow(hwnd(w));
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd(w)));
                if attached {
                    let _ = windows::Win32::System::Threading::AttachThreadInput(current, foreground_thread, false);
                }
                if GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) != HWND(canvas_input.handle as *mut _) {
                    return Err(format!("文字参数验收未进入自有贴图窗口链：foreground={:?}, root={:?}, popup={:?}, owner={:?}", GetForegroundWindow(), GetAncestor(GetForegroundWindow(), GA_ROOTOWNER), hwnd(w), HWND(canvas_input.handle as *mut _)).into());
                }
                Ok(())
            }}).map_err(|e| e.to_string())??;
            property(pin, "font-family-menu", false, cx).await?;
            if inspect(pin, cx, |v| v.toolbar_panel)? != crate::toolbar_layout::ToolbarPanel::FontFamily
                || bar.update(cx, |v, _, cx| v.popup(cx).is_none()).map_err(|e| e.to_string())? {
                return Err("旧字号弹窗恢复焦点导致新字体弹窗自行关闭".into());
            }
            popup_button(pin, "font-yahei", cx).await?;
            property(pin, "font-menu", false, cx).await?;
            popup_button(pin, "font40", cx).await?;
            if !pin.update(cx, |_, w, _| unsafe {
                windows::Win32::UI::Input::KeyboardAndMouse::GetFocus() == hwnd(w)
            }).map_err(|e| e.to_string())? {
                return Err("选完文字参数未恢复宿主原生输入焦点".into());
            }
            canvas_input.key(0x20);
            unsafe { PostMessageW(Some(HWND(canvas_input.handle as *mut _)), WM_CHAR, WPARAM(0x20), LPARAM(0))?; }
            pause(cx).await;
            if !pin.update(cx, |v, _, _| v.chrome).map_err(|e| e.to_string())? || inspect(pin, cx, |v| v.editor.text_draft.as_ref().map(|d| d.annotation.text.clone()))? != Some("word ".into()) { return Err("文字输入空格错误收起工具栏/未输入空格".into()); }
            canvas_input.key(0x1b);
            pause(cx).await;
            if !pin.update(cx, |v, _, _| v.chrome).map_err(|e| e.to_string())? || !inspect(pin, cx, |v| v.editor.text_draft.is_none() && v.editor.document.items().iter().any(|o| o.annotation.tool == Tool::Text && o.annotation.text == "word "))? { return Err("文字编辑 Esc 未提交当前内容并保留工具栏".into()); }
            button(pin, "mosaic", cx).await?;
            let mut expected = style(pin, cx)?;
            expected.block_size += 1;
            property(pin, "block-menu", true, cx).await?;
            require_style(style(pin, cx)?, expected, "马赛克强度滚轮")?;
            property(pin, "block-menu", false, cx).await?;
            popup_button(pin, "block32", cx).await?;
            expected.block_size = 32;
            require_style(style(pin, cx)?, expected, "马赛克强度弹窗")?;
            if bar_rect(pin, cx)? != anchor { return Err("马赛克参数弹窗改变主栏锚点".into()); }
            snapshot(properties_rect(pin,cx)?, "mosaic-properties.png")?;
            let main_input = bar_input(pin,cx)?;
            let parameter_input = properties_input(pin,cx)?;
            if main_input.handle == parameter_input.handle || main_input.handle == canvas_input.handle
                || anchor.height as f32 > 52.*main_input.scale {return Err("一级二级未拆成紧凑独立工具栏".into());}
            button(pin,"move",cx).await?;
            let main = pin.update(cx,|v,_,_|v.toolbar.unwrap()).map_err(|e|e.to_string())?;
            if main.update(cx,|v,_,_|v.properties.is_some()).map_err(|e|e.to_string())? {return Err("移动模式仍显示无关二级属性栏".into());}
            button(pin,"mosaic",cx).await?;
            if bar_rect(pin,cx)? != anchor {return Err("二级栏隐藏/重开挪动了一级工具栏".into());}
            property(pin, "block-menu", false, cx).await?;
            let bar = pin.update(cx, |v, _, _| v.toolbar.unwrap()).map_err(|e|e.to_string())?;
            let popup = bar.update(cx, |v,_,cx|v.popup(cx).unwrap()).map_err(|e|e.to_string())?;
            popup.update(cx, |_,w,_|unsafe{PostMessageW(Some(hwnd(w)),WM_CLOSE,WPARAM(0),LPARAM(0))}).map_err(|e|e.to_string())??;
            pause(cx).await;
            if inspect(pin,cx,|v|v.toolbar_panel)? != crate::toolbar_layout::ToolbarPanel::None {return Err("原生关闭参数窗未清理面板状态".into());}
            property(pin,"block-menu",false,cx).await?;
            if bar.update(cx,|v,_,cx|v.popup(cx).is_none()).map_err(|e|e.to_string())? {return Err("原生关闭后参数窗无法重新打开".into());}
            bar.update(cx,|_,w,_|unsafe{PostMessageW(Some(hwnd(w)),WM_CLOSE,WPARAM(0),LPARAM(0))}).map_err(|e|e.to_string())??;
            pause(cx).await;
            if pin.update(cx,|v,_,_|v.chrome||v.toolbar.is_some()).map_err(|e|e.to_string())? {return Err("原生关闭工具栏遗留活动标注状态".into());}
            canvas_input.key(0x20); pause(cx).await;
            if !pin.update(cx,|v,_,_|v.chrome&&v.toolbar.is_some()).map_err(|e|e.to_string())? {return Err("原生关闭后空格无法重新打开工具栏".into());}
            action(pin, PinAction::Close, cx)?;
            pause(cx).await;
            println!("{{\"test\":\"native_pin_shared_properties_targeted_wheel_popup_stable_anchor\",\"ok\":true}}");
            if std::env::var("RUSTSHOT_PIN_DETAILS_ONLY_PROPERTIES").as_deref() == Ok("1") {
                if cx.update(|cx| cx.windows().len()).map_err(|e| e.to_string())? != 1 {
                    return Err("属性专项遗留工具栏/弹窗或贴图窗口".into());
                }
                return Ok(());
            }
            }

            let (pin, _) = fresh(cx).await?;
            button(pin, "pen", cx).await?;
            property(pin, "green", false, cx).await?;
            property(pin, "width-menu", false, cx).await?;
            popup_button(pin, "thin", cx).await?;
            let pen_style = style(pin, cx)?;
            button(pin, "rectangle", cx).await?;
            property(pin, "blue", false, cx).await?;
            property(pin, "width-menu", false, cx).await?;
            popup_button(pin, "thick", cx).await?;
            let rectangle_style = style(pin, cx)?;
            pin.update(cx, |v, _, cx| v.output(cx)).map_err(|e| e.to_string())??;
            action(pin, PinAction::Hide, cx)?;
            let (second, _) = fresh(cx).await?;
            button(second, "pen", cx).await?;
            require_style(style(second, cx)?, pen_style, "输出后新贴图铅笔预设")?;
            button(second, "rectangle", cx).await?;
            require_style(style(second, cx)?, rectangle_style, "输出后新贴图矩形预设")?;
            button(second, "pen", cx).await?;
            property(second, "purple", false, cx).await?;
            property(second, "width-menu", false, cx).await?;
            popup_button(second, "largebrush", cx).await?;
            let closed_pen_style = style(second, cx)?;
            action(second, PinAction::Close, cx)?;
            pause(cx).await;
            let (third, _) = fresh(cx).await?;
            button(third, "pen", cx).await?;
            require_style(style(third, cx)?, closed_pen_style, "关闭后新贴图铅笔预设")?;
            button(third, "rectangle", cx).await?;
            require_style(style(third, cx)?, rectangle_style, "关闭后矩形预设隔离")?;
            action(third, PinAction::Close, cx)?;
            action(pin, PinAction::Close, cx)?;
            pause(cx).await;
            println!("{{\"test\":\"native_pin_tool_presets_output_close_reload_and_tool_isolation\",\"ok\":true}}");
            if cx.update(|cx| cx.windows().len()).map_err(|e| e.to_string())? != 1 { return Err("贴图细节专项遗留主栏/参数弹窗或贴图窗口".into()); }
            Ok(())
        }.await;
        // 只从本专项拥有的前台窗口恢复；用户自行切换应用后不再改变其焦点。
        unsafe {
            let mut owner = 0;
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut owner));
            if owner == std::process::id() && previous_foreground != 0 {
                let _ = SetForegroundWindow(HWND(previous_foreground as *mut _));
            }
        }
        if let Err(e) = result { *errors.borrow_mut() = Some(format!("贴图细节专项：{e}")); let _ = cx.update(|cx| cx.quit()); }
        else {
            // 最后一个 HWND 可能触发 GPUI 退出；先写完成哨兵，关闭之后不再 await。
            *errors.borrow_mut() = None;
            let _ = keepalive.update(cx, |v, w, cx| v.perform(PinAction::Close, w, cx));
            let _ = cx.update(|cx| cx.quit());
        }
    }).detach();
}
