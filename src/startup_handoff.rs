//! 首屏交接只传完成的选区；仍在拖动时原生首屏继续接收输入。
use crate::{
    capture_view::{Action, CaptureView},
    gpui_capture::hwnd,
    startup_output::StartupOutputKind,
    startup_surface::StartupSurface,
    startup_surface_input::ReplayInput,
    Result,
};
use gpui::{App, AppContext, Entity, WeakEntity, Window};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use windows::core::w;
use windows::Win32::{
    Foundation::*,
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::{GetCapture, ReleaseCapture},
        WindowsAndMessaging::*,
    },
};

#[link(name = "dwmapi")]
extern "system" {
    fn DwmFlush() -> i32;
}

pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    surface: StartupSurface,
    started: Instant,
    probe: bool,
    errors: Rc<RefCell<Option<String>>>,
) {
    let weak = view.downgrade();
    // 帧回调先于 draw，导入稳定选区后由这次绘制生成完整 GPUI 首帧。
    poll(weak, window, surface, started, probe, errors);
}

fn fail(
    window: &Window,
    surface: &StartupSurface,
    errors: &RefCell<Option<String>>,
    error: impl ToString,
) {
    *errors.borrow_mut() = Some(error.to_string());
    let _ = surface.dismiss();
    unsafe {
        let _ = PostMessageW(Some(hwnd(window)), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

fn poll(
    weak: WeakEntity<CaptureView>,
    window: &mut Window,
    surface: StartupSurface,
    started: Instant,
    probe: bool,
    errors: Rc<RefCell<Option<String>>>,
) {
    window.on_next_frame(move |window, cx| {
        if surface.snapshot().cancelled {
            let _ = surface.dismiss();
            let _ = weak.update(cx, |view, cx| view.perform(Action::Cancel, window, cx));
            return;
        }
        let adopted = match surface.handoff(hwnd(window)) {
            Ok(state) => state,
            Err(error) => {
                if surface.snapshot().cancelled {
                    let _ = surface.dismiss();
                    let _ = weak.update(cx, |view, cx| view.perform(Action::Cancel, window, cx));
                } else {
                    fail(window, &surface, &errors, error);
                }
                return;
            }
        };
        let Some(state) = adopted else {
            poll(weak, window, surface, started, probe, errors);
            return;
        };
        if state.cancelled {
            let _ = surface.dismiss();
            let _ = weak.update(cx, |view, cx| view.perform(Action::Cancel, window, cx));
            return;
        }
        let serial = unsafe { GetPropW(hwnd(window), w!("RustShot.GpuiPresentSerial")) }.0 as usize;
        if let Err(error) = weak
            .update(cx, |view, cx| -> Result<()> {
                if state.dragging
                    || state.crop.is_some_and(|crop| {
                        !crop.valid(view.source.bounds.width, view.source.bounds.height)
                    })
                    || state.manual_preview.is_some_and(|rect| {
                        state.crop.is_some()
                            || !rect.valid(view.source.bounds.width, view.source.bounds.height)
                    })
                {
                    return Err("快速框选尚未完成或选区超出冻结画面".into());
                }
                view.editor.set_crop(state.crop);
                view.set_manual_preview(state.manual_preview);
                view.wheel_input = state.wheel_input.clone();
                view.startup_click_seed = state.click_seed;
                view.pointer = state.pointer;
                view.pointer_sample = crate::annotation::Point {
                    x: state.pointer.0 as f32,
                    y: state.pointer.1 as f32,
                };
                view.toolbar_monitor = state
                    .toolbar_monitor
                    .min(view.source.screen_count().saturating_sub(1));
                cx.notify();
                Ok(())
            })
            .map_err(|e| e.to_string())
            .and_then(|result| result.map_err(|e| e.to_string()))
        {
            fail(window, &surface, &errors, error);
            return;
        }
        // 这次 draw 会包含刚导入的选区；下一次回调再确认 DWM 已呈现它。
        wait_for_present(
            PresentWait {
                weak,
                surface,
                started,
                probe,
                errors,
                serial,
                since: Instant::now(),
            },
            window,
        );
    });
    window.refresh();
}

struct PresentWait {
    weak: WeakEntity<CaptureView>,
    surface: StartupSurface,
    started: Instant,
    probe: bool,
    errors: Rc<RefCell<Option<String>>>,
    serial: usize,
    since: Instant,
}

fn wait_for_present(wait: PresentWait, window: &mut Window) {
    window.on_next_frame(move |window, cx| {
        let PresentWait {
            weak,
            surface,
            started,
            probe,
            errors,
            serial,
            since,
        } = wait;
        if surface.snapshot().cancelled {
            let _ = surface.dismiss();
            let _ = weak.update(cx, |view, cx| view.perform(Action::Cancel, window, cx));
            return;
        }
        let rendered = weak
            .update(cx, |view, _| {
                let frame =
                    unsafe { GetPropW(hwnd(window), w!("RustShot.GpuiPresentSerial")) }.0 as usize;
                let frame_size = unsafe { GetPropW(hwnd(window), w!("RustShot.GpuiPresentSize")) }.0
                    as usize as u32;
                frame != serial
                    && frame != 0
                    && frame_size == ((view.source.bounds.width << 16) | view.source.bounds.height)
            })
            .unwrap_or(false);
        if !rendered {
            if since.elapsed() >= Duration::from_secs(2) {
                fail(
                    window,
                    &surface,
                    &errors,
                    "GPUI 未能呈现完整截图画面，已取消本次截图",
                );
            } else {
                wait_for_present(
                    PresentWait {
                        weak,
                        surface,
                        started,
                        probe,
                        errors,
                        serial,
                        since,
                    },
                    window,
                );
            }
            return;
        }
        let validated = weak
            .update(cx, |view, _| -> Result<()> {
                let bounds = view.source.bounds;
                let native = hwnd(window);
                let mut client = RECT::default();
                let mut outer = RECT::default();
                unsafe {
                    GetClientRect(native, &mut client)?;
                    GetWindowRect(native, &mut outer)?;
                }
                let scale = window.scale_factor();
                let viewport = window.viewport_size();
                if outer.left != bounds.left
                    || outer.top != bounds.top
                    || client.right - client.left != bounds.width as i32
                    || client.bottom - client.top != bounds.height as i32
                    || (f32::from(viewport.width) * scale - bounds.width as f32).abs() > 1.
                    || (f32::from(viewport.height) * scale - bounds.height as f32).abs() > 1.
                    || (unsafe { GetDpiForWindow(native) } as f32 / 96. - scale).abs() > 0.001
                {
                    return Err("快速框选交接时 GPUI 视口与物理屏幕不一致".into());
                }
                let status = unsafe { DwmFlush() };
                if status < 0 {
                    return Err(
                        format!("快速框选交接 DwmFlush 失败：0x{:08x}", status as u32).into(),
                    );
                }
                Ok(())
            })
            .map_err(|e| e.to_string())
            .and_then(|result| result.map_err(|e| e.to_string()));
        if let Err(error) = validated {
            fail(window, &surface, &errors, error);
            return;
        }
        let native = hwnd(window).0 as usize;
        let own_window = window.window_handle();
        let scale = window.scale_factor();
        let mut async_cx = cx.to_async();
        // 原生焦点事件可同步重入 GPUI；在帧回调和 App 借用结束后完成切换。
        cx.foreground_executor()
            .spawn(async move {
                if surface.snapshot().cancelled {
                    let _ = surface.dismiss();
                    let _ = async_cx.update_window(own_window, |_, window, cx| {
                        let _ =
                            weak.update(cx, |view, cx| view.perform(Action::Cancel, window, cx));
                    });
                    return;
                }
                // 延迟任务只触碰仍属于本会话的原生窗口，不能作用于已关闭/复用的 HWND。
                let alive = async_cx
                    .update_window(own_window, |_, window, _| hwnd(window).0 as usize == native)
                    .unwrap_or(false);
                let mut owner = 0;
                unsafe {
                    GetWindowThreadProcessId(HWND(native as *mut _), Some(&mut owner));
                }
                if !alive
                    || owner != std::process::id()
                    || !unsafe { IsWindow(Some(HWND(native as *mut _))) }.as_bool()
                {
                    let _ = surface.dismiss();
                    return;
                }
                unsafe {
                    let target = HWND(native as *mut _);
                    let _ = SetWindowPos(
                        target,
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
                    );
                    let _ = SetForegroundWindow(target);
                }
                let handoff_result = surface.finish_handoff(scale).map_err(|e| e.to_string());
                if surface.snapshot().cancelled {
                    let _ = async_cx.update_window(own_window, |_, window, cx| {
                        let _ =
                            weak.update(cx, |view, cx| view.perform(Action::Cancel, window, cx));
                    });
                    return;
                }
                if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
                    println!(
                        "{{\"phase\":\"startup_surface_handoff\",\"elapsed_ms\":{:.3},\"ok\":{}}}",
                        started.elapsed().as_secs_f64() * 1000.,
                        handoff_result.is_ok()
                    );
                }
                match handoff_result {
                    Ok(events) => {
                        let _ = async_cx.update_window(own_window, |_, _, cx| {
                            let _ = weak.update(cx, |view, cx| {
                                view.control_selection_ready = true;
                                view.request_control_probe(cx);
                                cx.notify();
                            });
                        });
                        // 旧原生窗口销毁后重申 GPUI 的缓存光标；此处所有 App/Window 借用已结束。
                        // 鼠标静止时也执行，不注入鼠标移动；backend 仍核验实际命中及跨线程捕获。
                        unsafe {
                            let target = HWND(native as *mut _);
                            let mut point = POINT::default();
                            if GetForegroundWindow() == target
                                && GetCursorPos(&mut point).is_ok()
                                && WindowFromPoint(point) == target
                            {
                                let _ = SendMessageW(
                                    target,
                                    WM_SETCURSOR,
                                    Some(WPARAM(native)),
                                    Some(LPARAM(HTCLIENT as isize)),
                                );
                            }
                        }
                        let mut preview_replay_gesture = None;
                        for event in events {
                            if async_cx
                                .update_window(own_window, |_, window, cx| {
                                    match event {
                                        ReplayInput::Event(mut input) => {
                                            let event_gesture = input.preview_gesture_id();
                                            let left_down = matches!(&input.event, gpui::PlatformInput::MouseDown(down) if down.button == gpui::MouseButton::Left);
                                            let left_up = matches!(&input.event, gpui::PlatformInput::MouseUp(up) if up.button == gpui::MouseButton::Left);
                                            // 被 typed 原生范围接受的 Middle 仍按原序清 seed，
                                            // 即使随后编辑撤销 output 也不能重走 confirmed/preview Pin。
                                            let consumed_middle =
                                                input.native_preview_middle_consumed();
                                            let consumed_confirm =
                                                input.native_preview_confirm_consumed();
                                            let consumed_text = input.native_text_consumed();
                                            let monitor_snap = input.native_monitor_snap();
                                            let preview_wheel = input.native_preview_wheel_notches();
                                             let preview_hover = input.native_preview_hover();
                                            let text_replay = input.native_text_shortcut_seen();
                                            let enter_replay = matches!(
                                                &input.event,
                                                gpui::PlatformInput::KeyDown(down)
                                                    if down.keystroke.key == "enter"
                                            );
                                            let middle_replay = matches!(
                                                &input.event,
                                                gpui::PlatformInput::MouseDown(down)
                                                    if down.button == gpui::MouseButton::Middle
                                            );
                                            let cursor_replay = matches!(
                                                &input.event,
                                                gpui::PlatformInput::KeyDown(down)
                                                    if crate::capture_cursor_nudge::is_preview_movement_key(&down.keystroke.key)
                                            );
                                            let _ = weak.update(cx, |view, _| {
                                                if input.breaks_click_sequence() {
                                                    view.invalidate_click_sequence();
                                                }
                                                view.replay_canvas_click = input
                                                    .prepare_canvas_click(
                                                        &mut view.startup_click_seed,
                                                    );
                                                view.preview_middle_replay =
                                                    middle_replay && !consumed_middle;
                                                view.preview_enter_replay =
                                                    enter_replay && !consumed_confirm;
                                                view.preview_cursor_replay = cursor_replay;
                                                view.preview_text_replay = text_replay && !consumed_text;
                                            });
                                            if consumed_middle || consumed_confirm || consumed_text {
                                                return;
                                            }
                                            if monitor_snap == Some(crate::startup_surface_input::NativeMonitorSnap::Applied) {
                                                // 导入状态已含原生的最终固定范围，不把旧 Down 再切一次。
                                                return;
                                            }
                                            if let Some(notches) = preview_wheel {
                                                if let gpui::PlatformInput::ScrollWheel(event) = &input.event {
                                                    let _ = weak.update(cx, |view, cx| {
                                                        // 只借事件当时位置走现有完整 UIA probe 门；拒绝不降级到参数调整。
                                                        if view.editor.crop.is_some() {
                                                            return;
                                                        }
                                                        view.pointer_sample = view.world_point(event.position, window);
                                                        view.pointer = view.physical(event.position, window);
                                                        view.invalidate_control_hover();
                                                        view.wheel_input.reset_selection_center();
                                                        if view.navigate_control_selection(notches, cx) {
                                                            view.wheel_input.reset();
                                                        }
                                                    });
                                                }
                                                return;
                                            }
                                             if preview_hover
                                                 && weak.update(cx, |view, _| view.editor.crop.is_some()).unwrap_or(true)
                                             {
                                                 // Native hover 只补回同一无选区预览序列，不能作用于之后新建的 crop。
                                                 return;
                                             }
                                            if matches!(
                                                &input.event,
                                                gpui::PlatformInput::MouseMove(_)
                                                    | gpui::PlatformInput::MouseUp(_)
                                            ) {
                                                let scale = window.scale_factor();
                                                let _ = weak.update(cx, |view, _| {
                                                    if view.manual_preview_click.is_some()
                                                        && preview_replay_gesture.is_some()
                                                        && preview_replay_gesture == event_gesture
                                                    {
                                                        if let Some(start) = view.pointer_down_start {
                                                            view.manual_preview_dragged |= input.crossed_preview_drag(start, scale);
                                                        }
                                                    }
                                                });
                                            }
                                            if matches!(
                                                input.event,
                                                gpui::PlatformInput::MouseMove(_)
                                            ) {
                                                // 先结束 Entity 借用，再派发真实坐标；不重放虚构的远移点。
                                                let _ = weak.update(cx, |view, _| {
                                                    view.selection_pointer_dragged |= input
                                                        .crossed_click_tolerance(
                                                            view.pointer_down_start,
                                                        );
                                                });
                                            }
                                            // 只覆盖实际派发区间；Native 当时的 P/C 不借后来的放大镜重解释。
                                            let _ = weak.update(cx, |view, _| {
                                                view.capture_color_replay = true;
                                                view.manual_preview_replay_point = match monitor_snap {
                                                    Some(crate::startup_surface_input::NativeMonitorSnap::Pending { global_point, shown_preview }) => Some(crate::capture_view::ManualPreviewReplay { global_point, shown_preview }),
                                                    _ => None,
                                                };
                                            });
                                            if let gpui::PlatformInput::KeyDown(key) = input.event {
                                                window.dispatch_keystroke(key.keystroke, cx);
                                            } else {
                                                window.dispatch_event(input.event, cx);
                                            }
                                            if left_down {
                                                preview_replay_gesture = event_gesture.filter(|_| {
                                                    weak.update(cx, |view, _| view.manual_preview_click.is_some()).unwrap_or(false)
                                                });
                                            } else if left_up {
                                                preview_replay_gesture = None;
                                            }
                                            // 控件可能消费 Down，未进入 View::down 的证明也不可遗留。
                                            let _ = weak.update(cx, |view, _| {
                                                view.replay_canvas_click = None;
                                                view.preview_cursor_replay = false;
                                                view.preview_text_replay = false;
                                                view.capture_color_replay = false;
                                                view.manual_preview_replay_point = None;
                                            });
                                            if middle_replay {
                                                let _ = weak.update(cx, |view, _| {
                                                    view.preview_middle_replay = false;
                                                });
                                            }
                                            if enter_replay {
                                                let _ = weak.update(cx, |view, _| {
                                                    view.preview_enter_replay = false;
                                                });
                                            }
                                        }
                                        ReplayInput::CancelPointerGesture => {
                                            preview_replay_gesture = None;
                                            let cancelled = weak
                                                .update(cx, |view, cx| {
                                                    if view.pointer_down_start.is_none() {
                                                        return false;
                                                    }
                                                    view.editor.cancel_gesture();
                                                    view.startup_click_seed = None;
                                                    view.clear_pointer_tracking();
                                                    view.wheel_input.reset();
                                                    cx.notify();
                                                    true
                                                })
                                                .unwrap_or(false);
                                            // 捕获释放可能同步回调；不得仍持有 CaptureView 的可变借用。
                                            if cancelled && unsafe { GetCapture() } == hwnd(window)
                                            {
                                                unsafe {
                                                    let _ = ReleaseCapture();
                                                }
                                            }
                                        }
                                        ReplayInput::Output(output) => {
                                            let _ = weak.update(cx, |view, cx| {
                                                if let Some((kind, rect, original_crop)) = output.text() {
                                                    view.startup_text_output(kind, rect, original_crop, window, cx);
                                                } else if output.native_confirmed().is_some() {
                                                    // 原事件发生时 Native 没有工具栏，不能在晚到的 GPUI Chrome 上重命中。
                                                    if output.accepts(view.editor.crop, view.screenshot_output_ready()) {
                                                        view.perform(Action::Pin, window, cx);
                                                    }
                                                } else if let Some((rect, _original_point)) = output.manual_preview()
                                                {
                                                    // 固定/自动范围保留独立来源；两者都贴原已呈现框，不按晚到点重命中。
                                                    if view.manual_preview != Some(rect)
                                                        || !view.screenshot_preview_confirm_ready(rect)
                                                    {
                                                        return;
                                                    }
                                                    match view.output_rect_for(rect) {
                                                        Ok(output_rect) => view.pin_range(output_rect, window, cx),
                                                        Err(error) => {
                                                            view.status = error.to_string();
                                                            cx.notify();
                                                        }
                                                    }
                                                } else if let Some((rect, original_point)) =
                                                    output.preview()
                                                {
                                                    if !view.screenshot_preview_output_ready(
                                                        rect,
                                                        original_point,
                                                    ) {
                                                        return;
                                                    }
                                                    match view.output_rect_for(rect) {
                                                        Ok(output_rect) => {
                                                            view.pin_range(output_rect, window, cx)
                                                        }
                                                        Err(error) => {
                                                            view.status = error.to_string();
                                                            cx.notify();
                                                        }
                                                    }
                                                } else if let Some(rect) = output.preview_confirm()
                                                {
                                                    if !view.screenshot_preview_confirm_ready(rect)
                                                    {
                                                        return;
                                                    }
                                                    match view.output_rect_for(rect) {
                                                        Ok(_output_rect) => {
                                                            view.confirm_range(rect, window, cx)
                                                        }
                                                        Err(error) => {
                                                            view.status = error.to_string();
                                                            cx.notify();
                                                        }
                                                    }
                                                } else if output.accepts(
                                                    view.editor.crop,
                                                    view.screenshot_output_ready(),
                                                ) {
                                                    view.perform(
                                                        match output.kind {
                                                            StartupOutputKind::Pin => Action::Pin,
                                                            StartupOutputKind::Confirm => {
                                                                Action::Confirm
                                                            }
                                                            StartupOutputKind::ControlText | StartupOutputKind::Ocr => return,
                                                        },
                                                        window,
                                                        cx,
                                                    );
                                                }
                                            });
                                        }
                                    }
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        *errors.borrow_mut() = Some(error);
                        unsafe {
                            let _ = PostMessageW(
                                Some(HWND(native as *mut _)),
                                WM_CLOSE,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                        return;
                    }
                }
                if probe {
                    // 创建完成的 GPUI 窗口仍走原有首帧、焦点、ESC 和互斥体回收验收。
                    // 不把原生首屏指标冒充完整 GPUI 的启动耗时。
                    let _ = async_cx.update_window(own_window, |_, window, cx| {
                        if let Some(view) = weak.upgrade() {
                            crate::startup_probe::schedule(&view, window, cx, started, errors);
                        }
                    });
                }
            })
            .detach();
    });
    window.refresh();
}
