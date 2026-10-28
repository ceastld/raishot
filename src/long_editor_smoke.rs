//! 同窗长图剪辑的自有输入与成品检查，以及条带回退窗的共享内存回收。
use crate::{
    annotation::{Point, Tool},
    capture::Image,
    capture_view::{Action, CaptureView},
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{isolate_input, Input, SMOKE_MOVE},
    long_editor::{LongEditor, LongEditorOutcome},
    process_memory::{cpu_memory, gpu_memory},
    stitch::{ScrollAxis, StitchSegment, StitchSnapshot},
    Result,
};
use gpui::{
    div, App, AppContext, Application, AsyncApp, Render, Window, WindowHandle, WindowOptions,
};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, WPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
        Input::KeyboardAndMouse::{GetCapture, SetActiveWindow},
        WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId, IsWindow, IsWindowVisible, PostMessageW,
            SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
            WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
        },
    },
};

const WIDTH: u32 = 400;
// 超过单边限制，生产 Crop 必须保留条带回退而不物化整张图片。
const HEIGHT: u32 = 66_000;
const WAIT: Duration = Duration::from_secs(15);
const MEMORY_ALLOWANCE: u64 = 192 * 1024 * 1024;
struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        div()
    }
}
fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(20))
        .await;
}
fn source() -> (StitchSnapshot, std::sync::Weak<Image>) {
    let mut bgra = Vec::with_capacity(WIDTH as usize * HEIGHT as usize * 4);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            bgra.extend_from_slice(&[x as u8, y as u8, (y >> 8) as u8, 255]);
        }
    }
    let image = Arc::new(Image {
        width: WIDTH,
        height: HEIGHT,
        bgra,
        readback_ms: 0.,
    });
    let weak = Arc::downgrade(&image);
    (
        StitchSnapshot {
            width: WIDTH,
            height: HEIGHT,
            axis: ScrollAxis::Vertical,
            viewport: Rect::full(WIDTH, 640),
            segments: vec![StitchSegment {
                image,
                source: Rect::full(WIDTH, HEIGHT),
                origin: (0, 0),
            }],
        },
        weak,
    )
}
async fn button(editor: WindowHandle<LongEditor>, id: &str, cx: &mut AsyncApp) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let click = editor.update(cx, |v, w, _| {
            v.ui_positions.borrow().get(id).copied().map(|p| {
                (
                    Input {
                        handle: hwnd(w).0 as usize,
                        scale: w.scale_factor(),
                    },
                    p,
                )
            })
        })?;
        if let Some((input, p)) = click {
            isolate_input(HWND(input.handle as *mut _))?;
            input.click(p);
            tick(cx).await;
            return Ok(());
        }
        ensure(
            Instant::now() < deadline,
            &format!("调整窗按钮未绘制：{id}"),
        )?;
        tick(cx).await;
    }
}
async fn height(editor: WindowHandle<LongEditor>, expected: u32, cx: &mut AsyncApp) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        if editor.update(cx, |v, _, _| v.document.height())? == expected {
            return Ok(());
        }
        ensure(Instant::now() < deadline, "调整窗操作未更新文档高度")?;
        tick(cx).await;
    }
}
async fn strip_cycle(memory: bool, cx: &mut AsyncApp) -> Result<Value> {
    let baseline = cpu_memory()?["private_bytes"]
        .as_u64()
        .ok_or("缺少基线内存")?;
    let (snapshot, source_weak) = cx.background_executor().spawn(async { source() }).await;
    let source_pointer = Arc::as_ptr(&snapshot.segments[0].image) as usize;
    let product = cx
        .update(|cx| {
            crate::long_output::export(snapshot, crate::long_output::LongOutput::Crop, cx)
        })?
        .await?;
    let crate::long_output::LongProduct::Strip(snapshot) = product else {
        return Err("超单边限制 Crop 没有保留条带回退".into());
    };
    let outcome = Rc::new(Cell::new(false));
    let signal = outcome.clone();
    let editor = cx.update(|cx| {
        LongEditor::open(
            snapshot,
            move |result, _| {
                signal.set(matches!(result, LongEditorOutcome::Return));
            },
            cx,
        )
    })??;
    button(editor, "long-edit-actual", cx).await?;
    let native = editor.update(cx, |v, w, _| {
        let snapshot = v.document.snapshot();
        ensure(
            snapshot
                .segments
                .iter()
                .all(|s| Arc::as_ptr(&s.image) as usize == source_pointer),
            "打开调整窗复制了原始条带",
        )?;
        Ok::<_, Box<dyn std::error::Error>>(hwnd(w).0 as usize)
    })??;
    let entity = editor.update(cx, |_, _, cx| cx.entity().downgrade())?;
    let mut peak = cpu_memory()?["private_bytes"]
        .as_u64()
        .ok_or("缺少内存读数")?;
    if !memory {
        let input = editor.update(cx, |v, w, _| {
            let scale = w.scale_factor();
            let x = v
                .viewport
                .window_point(Point {
                    x: WIDTH as f32 / 2.,
                    y: 0.,
                })
                .x;
            (
                Input {
                    handle: native,
                    scale,
                },
                (x.round() as u32, (140. * scale) as u32),
                (x.round() as u32, (220. * scale) as u32),
            )
        })?;
        input.0.drag(input.1, input.2);
        let deadline = Instant::now() + WAIT;
        let band = loop {
            if let Some(band) = editor.update(cx, |v, _, _| v.band)? {
                break band;
            }
            ensure(Instant::now() < deadline, "真实拖选未建立删段范围")?;
            tick(cx).await;
        };
        input.0.key(0x2e);
        height(editor, HEIGHT - (band.1 - band.0), cx).await?;
        let snapshot = editor.update(cx, |v, _, _| v.document.snapshot())?;
        let seam = crate::long_document::render_region(
            &snapshot,
            Rect {
                x: 0,
                y: band.0,
                width: 1,
                height: 1,
            },
            1,
            1,
        )?;
        ensure(
            seam.bgra == [0, band.1 as u8, (band.1 >> 8) as u8, 255],
            "删段接缝像素不正确",
        )?;
        drop(snapshot);
        button(editor, "long-edit-undo", cx).await?;
        height(editor, HEIGHT, cx).await?;
        button(editor, "long-edit-redo", cx).await?;
        height(editor, HEIGHT - (band.1 - band.0), cx).await?;
        button(editor, "long-edit-reset", cx).await?;
        height(editor, HEIGHT, cx).await?;
    }
    for id in ["long-edit-all", "long-edit-cross", "long-edit-actual"] {
        button(editor, id, cx).await?;
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            tick(cx).await;
            peak = peak.max(
                cpu_memory()?["private_bytes"]
                    .as_u64()
                    .ok_or("缺少内存读数")?,
            );
        }
    }
    let gpu = gpu_memory();
    button(editor, "long-edit-return", cx).await?;
    let deadline = Instant::now() + WAIT;
    while !outcome.get()
        || entity.upgrade().is_some()
        || source_weak.upgrade().is_some()
        || unsafe { IsWindow(Some(HWND(native as *mut _))) }.as_bool()
    {
        ensure(
            Instant::now() < deadline,
            "返回后调整窗、实体或原始条带未回收",
        )?;
        tick(cx).await;
    }
    ensure(
        peak <= baseline + WIDTH as u64 * HEIGHT as u64 * 4 + MEMORY_ALLOWANCE,
        "调整窗采样内存超过原始条带加192MiB余量",
    )?;
    Ok(
        json!({"width":WIDTH,"height":HEIGHT,"production_oversize_strip_fallback":true,"source_arc_shared":true,"native_delete_undo_redo_reset":!memory,
        "seam_pixel_verified":!memory,"real_zoom_buttons":true,"returned":true,"source_released":true,
        "hwnd_destroyed":true,"entity_released":true,"baseline_private_bytes":baseline,
        "sampled_peak_private_bytes":peak,"memory_allowance_bytes":MEMORY_ALLOWANCE,"gpu":gpu}),
    )
}

fn clip_input_state(editor: WindowHandle<CaptureView>, cx: &mut AsyncApp) -> Result<Value> {
    Ok(editor.update(cx, |v, w, _| {
        let native = hwnd(w);
        let ui = v.long_clip_ui.as_ref();
        let drag = ui.and_then(|ui| ui.drag.as_ref()).map(|drag| match drag {
            crate::long_clip_ui::ClipDrag::Select { .. } => "select",
            crate::long_clip_ui::ClipDrag::SelectionEdge { .. } => "selection-edge",
            crate::long_clip_ui::ClipDrag::Adjust { .. } => "adjust",
            crate::long_clip_ui::ClipDrag::Trim { .. } => "trim",
            crate::long_clip_ui::ClipDrag::Pan { .. } => "pan",
        });
        json!({"active":w.is_window_active(),"focused":v.focus.is_focused(w),
            "visible":unsafe{IsWindowVisible(native)}.as_bool(),"foreground_owned":unsafe{GetForegroundWindow()}==native,
            "capture_owned":unsafe{GetCapture()}==native,"tool":format!("{:?}",v.editor.tool),"toolbar_panel":format!("{:?}",v.toolbar_panel),
            "drag":drag,"selection":ui.and_then(|ui|ui.selection.as_ref()).map(|range|[range.start,range.end]),"anchor":ui.and_then(|ui|ui.anchor),
            "parameters_pending":ui.is_some_and(|ui|ui.parameters_pending),"draft":ui.is_some_and(|ui|ui.draft.is_some()),"space":ui.is_some_and(|ui|ui.space),
            "saving":v.saving,"processing":v.processing,"editor_dragging":v.editor.is_dragging(),"pending_drawing":v.editor.has_pending_drawing(),
            "text_draft":v.editor.text_draft.is_some(),"lasso_draft":v.editor.lasso_draft.is_some(),"status":v.status,
            "modifiers":format!("{:?}",w.modifiers()),"viewport":v.image_canvas.as_ref().map(|canvas|format!("{:?}",canvas.viewport))})
    })?)
}

async fn clip_presented(editor: WindowHandle<CaptureView>, cx: &mut AsyncApp) -> Result<()> {
    let done = Rc::new(Cell::new(false));
    let signal = done.clone();
    editor.update(cx, |_, w, _| {
        w.on_next_frame(move |w, _| {
            w.on_next_frame(move |_, _| signal.set(true));
            w.refresh();
        });
        w.refresh();
    })?;
    let deadline = Instant::now() + WAIT;
    while !done.get() {
        if Instant::now() >= deadline {
            return Err(format!("同窗两帧等待超时：{}", clip_input_state(editor, cx)?).into());
        }
        tick(cx).await;
    }
    Ok(())
}

/// 仅显式验收显示自己的生产窗口；不提升窗口层级，不绕过生产输入资格。
pub(crate) async fn prepare_clip_input(
    editor: WindowHandle<CaptureView>,
    label: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    eprintln!(
        "long-clip-smoke {label} before-activation {}",
        clip_input_state(editor, cx)?
    );
    editor.update(cx, |v, w, cx| -> Result<()> {
        unsafe {
            let native = hwnd(w);
            let mut process = 0;
            ensure(
                GetWindowThreadProcessId(native, Some(&mut process)) == GetCurrentThreadId()
                    && process == std::process::id(),
                "验收窗口不属于当前进程的UI线程",
            )?;
            SetWindowPos(
                native,
                None,
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_SHOWWINDOW,
            )?;
            // 只设同线程活动窗口；不请求后台应用成为系统前台，资格仍等实际回执。
            let _ = SetActiveWindow(native);
        }
        w.focus(&v.focus);
        w.refresh();
        cx.notify();
        Ok(())
    })??;
    let deadline = Instant::now() + WAIT;
    loop {
        let state = clip_input_state(editor, cx)?;
        // PostMessage只投递已知自有HWND；系统前台归属另作诊断，不替代生产门禁。
        if ["active", "focused", "visible"]
            .iter()
            .all(|key| state[*key] == true)
        {
            clip_presented(editor, cx).await?;
            let presented = clip_input_state(editor, cx)?;
            if ["active", "focused", "visible"]
                .iter()
                .all(|key| presented[*key] == true)
            {
                eprintln!("long-clip-smoke {label} ready {presented}");
                return Ok(());
            }
        }
        ensure(
            Instant::now() < deadline,
            &format!("同窗输入激活超时：{label} {state}"),
        )?;
        tick(cx).await;
    }
}

async fn clip_drag(
    editor: WindowHandle<CaptureView>,
    input: Input,
    points: [(u32, u32); 2],
    cx: &mut AsyncApp,
) -> Result<()> {
    eprintln!(
        "long-clip-smoke before-drag points={points:?} {}",
        clip_input_state(editor, cx)?
    );
    for (phase, message, flags, point) in [
        ("down", WM_LBUTTONDOWN, 1, points[0]),
        ("move", WM_MOUSEMOVE, SMOKE_MOVE | 1, points[1]),
        ("up", WM_LBUTTONUP, 0, points[1]),
    ] {
        clip_pointer_phase(editor, input, phase, message, flags, point, cx).await?;
    }
    Ok(())
}

async fn clip_pointer_phase(
    editor: WindowHandle<CaptureView>,
    input: Input,
    phase: &str,
    message: u32,
    flags: usize,
    point: (u32, u32),
    cx: &mut AsyncApp,
) -> Result<()> {
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            message,
            WPARAM(flags),
            LPARAM(((point.1 << 16) | point.0) as isize),
        )?;
    }
    tick(cx).await;
    clip_presented(editor, cx).await?;
    eprintln!(
        "long-clip-smoke after-{phase} {}",
        clip_input_state(editor, cx)?
    );
    Ok(())
}

async fn clip_place_boundary(
    editor: WindowHandle<CaptureView>,
    axis: ScrollAxis,
    long: u32,
    cx: &mut AsyncApp,
) -> Result<(u32, u32)> {
    let point = editor.update(cx, |v, w, cx| -> Result<_> {
        let dpi = w.scale_factor();
        let cross_offset = clip_cross_offset(axis, w, 160.);
        let canvas = v.image_canvas.as_mut().ok_or("裁边验收缺少视口")?;
        canvas.viewport.zoom = dpi;
        canvas.viewport.offset = Point {
            x: 60. * dpi,
            y: cross_offset,
        };
        canvas.auto_fit = false;
        let at = v.clip_view_point(axis_point(axis, long as f32, 160.), w);
        let canvas = v.image_canvas.as_mut().unwrap();
        match axis {
            ScrollAxis::Vertical => canvas.viewport.offset.y += 280. * dpi - at.y,
            ScrollAxis::Horizontal => canvas.viewport.offset.x += 280. * dpi - at.x,
        }
        let at = v.clip_view_point(axis_point(axis, long as f32, 160.), w);
        cx.notify();
        Ok((at.x.round() as u32, at.y.round() as u32))
    })??;
    clip_presented(editor, cx).await?;
    Ok(point)
}

fn clip_cross_offset(axis: ScrollAxis, window: &Window, source_cross: f32) -> f32 {
    let dpi = window.scale_factor();
    if axis == ScrollAxis::Vertical {
        return 120. * dpi;
    }
    let size = window.viewport_size();
    let area = crate::long_clip_navigation::editing_area(
        axis,
        f32::from(size.width),
        f32::from(size.height),
        true,
    );
    (area.y + area.height / 2. - source_cross) * dpi
}

fn clip_shift_point(axis: ScrollAxis, point: (u32, u32), delta: f32) -> (u32, u32) {
    match axis {
        ScrollAxis::Vertical => (point.0, (point.1 as f32 + delta).round() as u32),
        ScrollAxis::Horizontal => ((point.0 as f32 + delta).round() as u32, point.1),
    }
}

async fn clip_boundary_cancel(
    editor: WindowHandle<CaptureView>,
    axis: ScrollAxis,
    input: Input,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let original_marks = editor.update(cx, |v, _, cx| -> Result<_> {
        let marks = v.editor.annotations();
        v.editor.clear_long_clip()?;
        v.editor.trim_long_clip(100..5900, 0..320)?;
        let ui = v.long_clip_ui.as_mut().ok_or("裁边验收缺少输入状态")?;
        ui.expanded.clear();
        ui.selection = None;
        ui.anchor = None;
        cx.notify();
        Ok(marks)
    })??;
    for (long, delta, expected) in [
        (100, -60., vec![0..40, 5900..6000]),
        (5900, 10., vec![0..40, 5910..6000]),
    ] {
        let start = clip_place_boundary(editor, axis, long, cx).await?;
        let end = clip_shift_point(axis, start, delta * input.scale);
        clip_drag(editor, input, [start, end], cx).await?;
        editor.update(cx, |v, _, _| -> Result<()> {
            ensure(
                v.editor
                    .long_clip()
                    .is_some_and(|clip| clip.removed() == expected),
                "原生裁边向外恢复未精确保留拖动像素",
            )?;
            ensure(
                v.editor.annotations() == original_marks,
                "向外恢复移动或清除了原图标注",
            )
        })??;
    }
    let start = clip_place_boundary(editor, axis, 40, cx).await?;
    let (revision, clip, crop, anchor, before) = editor.update(cx, |v, w, _| -> Result<_> {
        let anchor = v.long_clip_source_center(w).ok_or("裁边取消缺少源锚")?;
        Ok((
            v.editor.document.revision(),
            v.editor.long_clip().cloned(),
            v.editor.crop,
            anchor,
            v.clip_view_point(anchor, w),
        ))
    })??;
    let end = clip_shift_point(axis, start, 40. * input.scale);
    clip_pointer_phase(editor, input, "cancel-down", WM_LBUTTONDOWN, 1, start, cx).await?;
    clip_pointer_phase(
        editor,
        input,
        "cancel-move",
        WM_MOUSEMOVE,
        SMOKE_MOVE | 1,
        end,
        cx,
    )
    .await?;
    ensure(
        editor.update(cx, |v, _, _| {
            v.long_clip_ui.as_ref().is_some_and(|ui| ui.draft.is_some())
        })?,
        "Esc验收未实际进入裁边草稿",
    )?;
    input.key(0x1b);
    tick(cx).await;
    clip_presented(editor, cx).await?;
    editor.update(cx, |v, w, _| -> Result<()> {
        let ui = v.long_clip_ui.as_ref().ok_or("Esc后剪辑输入丢失")?;
        ensure(
            ui.drag.is_none() && ui.draft.is_none(),
            "Esc未取消裁边手势和草稿",
        )?;
        ensure(
            v.editor.document.revision() == revision
                && v.editor.long_clip() == clip.as_ref()
                && v.editor.crop == crop
                && v.editor.annotations() == original_marks,
            "Esc裁边取消提交了历史或修改了原图标注",
        )?;
        let after = v.clip_view_point(anchor, w);
        ensure(
            (before.x - after.x).abs() < 1. && (before.y - after.y).abs() < 1.,
            "Esc裁边取消使源锚跳动",
        )
    })??;
    clip_pointer_phase(editor, input, "cancel-up", WM_LBUTTONUP, 0, end, cx).await?;
    let pan_start = editor.update(cx, |_, w, _| {
        let size = w.viewport_size();
        let area = crate::long_clip_navigation::editing_area(
            axis,
            f32::from(size.width),
            f32::from(size.height),
            true,
        );
        (
            ((area.x + area.width / 2.) * input.scale).round() as u32,
            ((area.y + area.height / 2.) * input.scale).round() as u32,
        )
    })?;
    let pan_end = clip_shift_point(axis, pan_start, 25. * input.scale);
    let initial_offset = editor.update(cx, |v, _, _| {
        v.image_canvas.as_ref().unwrap().viewport.offset
    })?;
    input.key(0x20);
    tick(cx).await;
    clip_presented(editor, cx).await?;
    clip_pointer_phase(editor, input, "pan-down", WM_LBUTTONDOWN, 1, pan_start, cx).await?;
    clip_pointer_phase(
        editor,
        input,
        "pan-move",
        WM_MOUSEMOVE,
        SMOKE_MOVE | 1,
        pan_end,
        cx,
    )
    .await?;
    let panned = editor.update(cx, |v, _, _| -> Result<_> {
        ensure(
            v.long_clip_ui.as_ref().is_some_and(|ui| {
                matches!(ui.drag, Some(crate::long_clip_ui::ClipDrag::Pan { .. }))
            }),
            "Space验收没有建立平移手势",
        )?;
        let offset = v.image_canvas.as_ref().unwrap().viewport.offset;
        ensure(offset != initial_offset, "Space平移没有改变浏览位置")?;
        Ok(offset)
    })??;
    input.key(0x1b);
    tick(cx).await;
    clip_presented(editor, cx).await?;
    ensure(
        editor.update(cx, |v, _, _| {
            v.long_clip_ui.as_ref().is_some_and(|ui| ui.drag.is_none())
                && v.image_canvas.as_ref().unwrap().viewport.offset == panned
                && v.editor.document.revision() == revision
        })?,
        "Space平移Esc取消回滚了浏览位置或提交了文档",
    )?;
    clip_pointer_phase(editor, input, "pan-up", WM_LBUTTONUP, 0, pan_end, cx).await?;
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            WM_KEYUP,
            WPARAM(0x20),
            LPARAM(0),
        )?;
    }
    for key in [0x25, 0x26, 0x27, 0x28] {
        input.key(key);
        tick(cx).await;
    }
    clip_presented(editor, cx).await?;
    ensure(
        editor.update(cx, |v, _, _| {
            v.editor.document.revision() == revision
                && v.editor.crop == crop
                && v.editor.long_clip() == clip.as_ref()
                && v.editor.annotations() == original_marks
        })?,
        "Crop无选段方向键修改了旧选区或文档",
    )?;
    Ok(
        json!({"prefix_outward_pixels":60,"suffix_outward_pixels":10,"annotation_source_coordinates_preserved":true,"trim_escape_no_history":true,"trim_escape_anchor_stable":true,"space_pan_escape_preserves_offset":true,"crop_direction_keys_isolated":true}),
    )
}

async fn clip_button(editor: WindowHandle<CaptureView>, id: &str, cx: &mut AsyncApp) -> Result<()> {
    let deadline = Instant::now() + WAIT;
    loop {
        let click = editor.update(cx, |v, w, _| {
            v.ui_positions.borrow().get(id).copied().map(|p| {
                (
                    Input {
                        handle: hwnd(w).0 as usize,
                        scale: w.scale_factor(),
                    },
                    p,
                )
            })
        })?;
        if let Some((input, p)) = click {
            isolate_input(HWND(input.handle as *mut _))?;
            input.click(p);
            tick(cx).await;
            // 返回会销毁此窗口；调用方用回调、Weak和原生HWND检查回收。
            if id != "long-clip-return" {
                clip_presented(editor, cx).await?;
            }
            return Ok(());
        }
        ensure(
            Instant::now() < deadline,
            &format!("同窗剪辑按钮未绘制：{id}"),
        )?;
        tick(cx).await;
    }
}

fn clip_source(axis: ScrollAxis) -> (StitchSnapshot, std::sync::Weak<Image>) {
    let (width, height) = match axis {
        ScrollAxis::Vertical => (320, 6_000),
        ScrollAxis::Horizontal => (6_000, 320),
    };
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            let (long, cross) = match axis {
                ScrollAxis::Vertical => (y, x),
                ScrollAxis::Horizontal => (x, y),
            };
            bgra.extend_from_slice(&[cross as u8, long as u8, (long >> 8) as u8, 255]);
        }
    }
    let image = Arc::new(Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    });
    let weak = Arc::downgrade(&image);
    (
        StitchSnapshot {
            width,
            height,
            axis,
            viewport: Rect::full(width.min(640), height.min(640)),
            segments: vec![StitchSegment {
                image,
                source: Rect::full(width, height),
                origin: (0, 0),
            }],
        },
        weak,
    )
}

fn axis_point(axis: ScrollAxis, long: f32, cross: f32) -> Point {
    match axis {
        ScrollAxis::Vertical => Point { x: cross, y: long },
        ScrollAxis::Horizontal => Point { x: long, y: cross },
    }
}

async fn clip_cycle(axis: ScrollAxis, cx: &mut AsyncApp) -> Result<Value> {
    let (snapshot, strip_weak) = cx
        .background_executor()
        .spawn(async move { clip_source(axis) })
        .await;
    let source_size = (snapshot.width, snapshot.height);
    let product = cx
        .update(|cx| {
            crate::long_output::export(snapshot, crate::long_output::LongOutput::Crop, cx)
        })?
        .await?;
    let crate::long_output::LongProduct::EditorImage {
        image,
        axis: product_axis,
    } = product
    else {
        return Err("限额内 Crop 没有进入同窗图片编辑器产品".into());
    };
    ensure(
        product_axis == axis && strip_weak.upgrade().is_none(),
        "Crop 物化后方向或旧条带释放错误",
    )?;
    let returned = Rc::new(Cell::new(false));
    let signal = returned.clone();
    let editor = cx.update(|cx| {
        crate::image_editor::open_long(
            image,
            axis,
            None,
            move |outcome, _| {
                signal.set(matches!(
                    outcome,
                    crate::long_output::LongOutcome::Return(None)
                ));
            },
            cx,
        )
    })??;
    prepare_clip_input(editor, "editor", cx).await?;
    clip_button(editor, "long-clip-actual", cx).await?;
    let (input, weak) = editor.update(cx, |v, w, cx| {
        let dpi = w.scale_factor();
        let cross_offset = clip_cross_offset(axis, w, 160.);
        let canvas = v.image_canvas.as_mut().ok_or("同窗剪辑缺少图片画布")?;
        canvas.viewport.zoom = dpi;
        canvas.viewport.offset = Point {
            x: 60. * dpi,
            y: cross_offset,
        };
        canvas.auto_fit = false;
        v.long_clip_ui.as_mut().ok_or("同窗剪辑缺少输入状态")?.snap = false;
        cx.notify();
        Ok::<_, Box<dyn std::error::Error>>((
            Input {
                handle: hwnd(w).0 as usize,
                scale: dpi,
            },
            cx.entity().downgrade(),
        ))
    })??;
    isolate_input(HWND(input.handle as *mut _))?;
    clip_presented(editor, cx).await?;
    let points = editor.update(cx, |v, w, _| {
        [60., 140.].map(|long| {
            let p = v.clip_view_point(axis_point(axis, long, 160.), w);
            (p.x.round() as u32, p.y.round() as u32)
        })
    })?;
    clip_drag(editor, input, points, cx).await?;
    let deadline = Instant::now() + WAIT;
    let band = loop {
        if let Some(range) = editor.update(cx, |v, _, _| {
            v.long_clip_ui.as_ref().and_then(|ui| ui.selection.clone())
        })? {
            if range.start < range.end {
                break range;
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "同窗真实拖选没有建立原图删段范围：{}",
                clip_input_state(editor, cx)?
            )
            .into());
        }
        tick(cx).await;
    };
    ensure(band == (60..140), "真实拖选没有保留原图坐标")?;
    input.key(0x2e);
    let deadline = Instant::now() + WAIT;
    loop {
        if editor.update(cx, |v, _, _| {
            v.editor
                .long_clip()
                .is_some_and(|clip| clip.removed() == [band.clone()])
        })? {
            break;
        }
        ensure(Instant::now() < deadline, "同窗 Delete 没有提交删段")?;
        tick(cx).await;
    }
    editor.update(cx, |v, _, _| -> Result<()> {
        let output = v.selected_content_image()?;
        let expected = match axis {
            ScrollAxis::Vertical => (320, 5_920),
            ScrollAxis::Horizontal => (5_920, 320),
        };
        ensure(
            (output.width, output.height) == expected,
            "同窗删段成品尺寸错误",
        )?;
        let (x, y) = match axis {
            ScrollAxis::Vertical => (0, band.start),
            ScrollAxis::Horizontal => (band.start, 0),
        };
        let at = (y as usize * output.width as usize + x as usize) * 4;
        ensure(
            output.bgra[at..at + 4] == [0, band.end as u8, (band.end >> 8) as u8, 255],
            "同窗成品接缝没有接到删除段之后的原图像素",
        )
    })??;
    let annotation_cross_end = editor.update(cx, |v, w, cx| {
        v.perform(Action::Tool(Tool::Rectangle), w, cx);
        v.editor.style.filled = true;
        let mut cross_end = 60.;
        if axis == ScrollAxis::Horizontal {
            let size = w.viewport_size();
            let area = crate::long_clip_navigation::editing_area(
                axis,
                f32::from(size.width),
                f32::from(size.height),
                true,
            );
            // Down 留在接缝标签下方；低窗缩短跨轴，两个端点均落在编辑区内。
            if area.height < 84. {
                cross_end = 40.;
            }
            v.image_canvas.as_mut().unwrap().viewport.offset.y =
                (area.y + 36. - 20.) * w.scale_factor();
        }
        cx.notify();
        cross_end
    })?;
    tick(cx).await;
    let points = editor.update(cx, |v, w, _| {
        [(40., 20.), (180., annotation_cross_end)].map(|(long, cross)| {
            let p = v.clip_view_point(axis_point(axis, long, cross), w);
            (p.x.round() as u32, p.y.round() as u32)
        })
    })?;
    input.stroke(&points)?;
    let deadline = Instant::now() + WAIT;
    loop {
        if editor.update(cx, |v, _, _| v.editor.annotations().len())? == 1 {
            break;
        }
        ensure(Instant::now() < deadline, "同窗标注手势没有提交")?;
        tick(cx).await;
    }
    editor.update(cx, |v, _, _| -> Result<()> {
        let marks = v.editor.annotations();
        let bounds = marks[0].local_bounds();
        let (start, end) = match axis {
            ScrollAxis::Vertical => (bounds.y, bounds.y + bounds.height),
            ScrollAxis::Horizontal => (bounds.x, bounds.x + bounds.width),
        };
        ensure(
            (start - 40.).abs() < 1. && (end - 180.).abs() < 1.,
            "跨接缝标注没有使用原图坐标",
        )?;
        let (cross_start, cross_end) = match axis {
            ScrollAxis::Vertical => (bounds.x, bounds.x + bounds.width),
            ScrollAxis::Horizontal => (bounds.y, bounds.y + bounds.height),
        };
        ensure(
            (cross_start - 20.).abs() < 1. && (cross_end - annotation_cross_end).abs() < 1.,
            "跨接缝标注的跨轴原图坐标错误",
        )?;
        ensure(
            (v.editor.canvas.width, v.editor.canvas.height) == source_size,
            "剪辑改变了原图画布",
        )
    })??;
    for (id, marks, removed) in [
        ("long-clip-undo", 0, 1),
        ("long-clip-undo", 0, 0),
        ("long-clip-redo", 0, 1),
        ("long-clip-redo", 1, 1),
    ] {
        clip_button(editor, id, cx).await?;
        ensure(
            editor.update(cx, |v, _, _| {
                v.editor.annotations().len() == marks
                    && v.editor
                        .long_clip()
                        .is_some_and(|clip| clip.removed().len() == removed)
            })?,
            "剪辑与标注没有进入同一撤销顺序",
        )?;
    }
    editor.update(cx, |v, w, cx| v.perform(Action::Tool(Tool::Crop), w, cx))?;
    let anchor = editor.update(cx, |v, w, _| {
        v.long_clip_source_center(w).ok_or("缺少展开锚点")
    })??;
    let before = editor.update(cx, |v, w, _| v.clip_view_point(anchor, w))?;
    for expanded in [true, false] {
        input.key(0x48);
        let deadline = Instant::now() + WAIT;
        loop {
            if editor.update(cx, |v, _, _| {
                v.long_clip_ui
                    .as_ref()
                    .is_some_and(|ui| !ui.expanded.is_empty())
                    == expanded
            })? {
                break;
            }
            ensure(Instant::now() < deadline, "H 没有切换删除段展开")?;
            tick(cx).await;
        }
        let after = editor.update(cx, |v, w, _| v.clip_view_point(anchor, w))?;
        ensure(
            (before.x - after.x).abs() < 1. && (before.y - after.y).abs() < 1.,
            "展开或收起使原图锚点跳动",
        )?;
    }
    let old_offset = editor.update(cx, |v, _, _| {
        v.image_canvas.as_ref().unwrap().viewport.offset
    })?;
    clip_button(editor, "long-clip-full-ruler", cx).await?;
    ensure(
        editor.update(cx, |v, _, _| {
            v.image_canvas.as_ref().unwrap().viewport.offset != old_offset
                && v.editor.annotations().len() == 1
                && v.editor.long_clip().unwrap().removed() == [band.clone()]
        })?,
        "全长导航没有移动视图或意外修改了文档",
    )?;
    let boundary_cancel = clip_boundary_cancel(editor, axis, input, cx).await?;
    clip_button(editor, "long-clip-return", cx).await?;
    let deadline = Instant::now() + WAIT;
    while !returned.get()
        || weak.upgrade().is_some()
        || unsafe { IsWindow(Some(HWND(input.handle as *mut _))) }.as_bool()
    {
        ensure(
            Instant::now() < deadline,
            "同窗继续采集后回调、实体或窗口未回收",
        )?;
        tick(cx).await;
    }
    Ok(
        json!({"axis":format!("{axis:?}"),"source_size":source_size,"production_crop_editor_image":true,"same_window_annotation":true,
        "native_delete":true,"source_coordinate_annotation":true,"mixed_undo_redo":true,"selected_content_seam":true,
        "native_navigation":true,"expand_anchor_stable":true,"boundary_cancel":boundary_cancel,"returned":true,"hwnd_destroyed":true,"entity_released":true}),
    )
}
fn execute(memory: bool) -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    if !memory {
        let case = std::env::var("RUSTSHOT_LONG_EDITOR_QA_CASE").unwrap_or_else(|_| "all".into());
        ensure(
            matches!(case.as_str(), "all" | "editor"),
            "当前调整窗专项只接受 all / editor；面板、图片与贴图使用各自专项",
        )?;
    }
    let variable = if memory {
        "RUSTSHOT_LONG_MEMORY_QA_DIR"
    } else {
        "RUSTSHOT_LONG_QA_DIR"
    };
    let root = std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?.join("target/qa/long-editor"));
    std::fs::create_dir_all(&root)?;
    std::env::set_var("RUSTSHOT_DATA_DIR", root.join("settings"));
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    let error = Rc::new(RefCell::new(Some("调整窗专项未完成".to_string())));
    let finished = error.clone();
    Application::new().with_assets(crate::toolbar_icons::IconAssets).run(move |cx: &mut App| {
        if let Err(e) = cx.open_window(WindowOptions { show: false, focus: false, ..Default::default() }, |_, cx| cx.new(|_| Host)) {
            *error.borrow_mut() = Some(e.to_string()); cx.quit(); return;
        }
        cx.spawn(async move |cx| {
            let result: Result<()> = async {
                let mut rounds = vec![];
                if memory {
                    for _ in 0..3 { rounds.push(strip_cycle(true, cx).await?); }
                } else {
                    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] { rounds.push(clip_cycle(axis, cx).await?); }
                }
                let report = json!({"ok":true,"suite":"long-editor","pid":std::process::id(),"owned_only":true,
                    "test":if memory { "adjustment_shared_strips_memory_recycled" } else { "adjustment_native_edit_zoom_return" },
                    "rounds":rounds,"clipboard_untouched":true,"desktop_pixels_read":false,
                    "memory_scope":if memory { "legacy strip window sampled private bytes; no full-image export" } else { "functional output pixels; memory gate belongs to memory suite" }});
                std::fs::write(root.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
                std::fs::write(root.join("ui.jsonl"), format!("{report}\n"))?;
                println!("{report}");
                Ok(())
            }.await;
            *error.borrow_mut() = result.err().map(|e| e.to_string());
            let _ = cx.update(|cx| { for w in cx.windows() { let _ = cx.update_window(w, |_, w, _| w.remove_window()); } cx.quit(); });
        }).detach();
    });
    let result = finished.borrow_mut().take();
    match result {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}
pub(crate) fn run() -> Result<()> {
    execute(false)
}
/// 开发对照：直接用指定图片打开同窗剪辑编辑器，关闭即退出，不经采集会话。
pub(crate) fn run_preview(path: &std::path::Path) -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let root = std::env::current_dir()?.join("target/qa/long-editor");
    std::fs::create_dir_all(&root)?;
    std::env::set_var("RUSTSHOT_DATA_DIR", root.join("settings"));
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    let image = crate::image_decode::decode_path(path)?.image;
    let axis = if image.width > image.height {
        ScrollAxis::Horizontal
    } else {
        ScrollAxis::Vertical
    };
    let error = Rc::new(RefCell::new(None::<String>));
    let failed = error.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            let opened = crate::image_editor::open_long(image, axis, None, |_, cx| cx.quit(), cx);
            match opened {
                // RUSTSHOT_LONG_CLIP_PREVIEW_DEMO：预置首部裁剪、一段删除和一个待删选段，便于对照界面。
                Ok(handle) if std::env::var_os("RUSTSHOT_LONG_CLIP_PREVIEW_DEMO").is_some() => {
                    let _ = handle.update(cx, |v, w, cx| {
                        let _ = v.editor.delete_long_clip_range(0..120);
                        let _ = v.editor.delete_long_clip_range(1400..1900);
                        if let Some(ui) = v.long_clip_ui.as_mut() {
                            ui.set_selection(2300, 2600);
                        }
                        v.long_clip_fit_width(w, cx);
                    });
                }
                Ok(_) => {}
                Err(e) => {
                    *failed.borrow_mut() = Some(e.to_string());
                    cx.quit();
                }
            }
        });
    let error = error.borrow_mut().take();
    error.map_or(Ok(()), |e| Err(e.into()))
}
pub(crate) fn run_memory() -> Result<()> {
    execute(true)
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "显式运行自有 GPUI 窗口"]
    fn native_long_clip_editor_smoke() {
        super::run().expect("长图编辑器原生验收失败");
    }
}
