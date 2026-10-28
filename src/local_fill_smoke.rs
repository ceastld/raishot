//! LocalFill 原生专项：只操作自有合成 Pin/工具栏，读取自有 GPU framebuffer。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    capture_view::{Action, CaptureView},
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_model::{PinState, Placement},
    pin_native as native,
    pin_view::{PinAction, PinView},
    Result,
};
use gpui::{App, AsyncApp, WindowHandle};
use std::{cell::RefCell, path::Path, rc::Rc, sync::Arc, time::Duration};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

const FOLDER: &str = "target/qa/local-fill";
const GREEN: [u8; 4] = [50, 160, 50, 255];
const PURPLE: [u8; 4] = [150, 60, 180, 255];
fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn fixture(width: u32, height: u32, letters: bool) -> Image {
    let mut image = Image {
        width,
        height,
        bgra: GREEN.repeat(width as usize * height as usize),
        readback_ms: 0.,
    };
    if letters {
        for y in 0..height {
            for x in width / 2..width {
                let i = (y as usize * width as usize + x as usize) * 4;
                image.bgra[i..i + 4].copy_from_slice(&PURPLE);
            }
        }
        // 完全自有的白色像素字形，避开字体/桌面输入依赖。
        for x in [75, 395] {
            for y in 97..103 {
                for col in x..x + 20 {
                    let i = (y as usize * width as usize + col as usize) * 4;
                    image.bgra[i..i + 4].copy_from_slice(&[255; 4]);
                }
            }
        }
    }
    image
}
fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
    let i = (y as usize * image.width as usize + x as usize) * 4;
    image.bgra[i..i + 4].try_into().unwrap()
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(60))
        .await;
}
async fn settle(cx: &mut AsyncApp) {
    for _ in 0..4 {
        tick(cx).await;
    }
}
fn inspect<T>(
    pin: WindowHandle<PinView>,
    cx: &mut AsyncApp,
    f: impl FnOnce(&CaptureView) -> T,
) -> Result<T> {
    pin.update(cx, |v, _, cx| -> Result<T> {
        Ok(f(v.canvas.as_ref().ok_or("专项画布不存在")?.read(cx)))
    })
    .map_err(|e| e.to_string())?
}
fn main_input(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Input> {
    pin.update(cx, |_, w, _| Input {
        handle: hwnd(w).0 as usize,
        scale: w.scale_factor(),
    })
    .map_err(|e| e.to_string().into())
}
fn bars(
    pin: WindowHandle<PinView>,
    cx: &mut AsyncApp,
) -> Result<(Input, Placement, Input, Placement)> {
    let primary = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("一级工具栏不存在")?;
    let (input, rect, secondary) = primary
        .update(cx, |v, w, _| -> Result<_> {
            Ok((
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
                native::rect(hwnd(w))?,
                v.properties.ok_or("二级工具栏不存在")?,
            ))
        })
        .map_err(|e| e.to_string())??;
    let (other, other_rect) = secondary
        .update(cx, |_, w, _| -> Result<_> {
            Ok((
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
                native::rect(hwnd(w))?,
            ))
        })
        .map_err(|e| e.to_string())??;
    for h in [input.handle, other.handle] {
        gpui_smoke::isolate_input(HWND(h as *mut _))?;
    }
    Ok((input, rect, other, other_rect))
}
async fn property_click(
    pin: WindowHandle<PinView>,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let point = inspect(pin, cx, |c| c.ui_positions.borrow().get(id).copied())?
        .ok_or_else(|| format!("副栏按钮未绘制：{id}"))?;
    bars(pin, cx)?.2.click(point);
    settle(cx).await;
    Ok(())
}
async fn size_preset(
    pin: WindowHandle<PinView>,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    property_click(pin, "width-menu", cx).await?;
    let primary = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("一级工具栏不存在")?;
    let popup = primary
        .update(cx, |v, _, cx| v.popup(cx))
        .map_err(|e| e.to_string())?
        .ok_or("大小弹窗未打开")?;
    let point = inspect(pin, cx, |c| c.ui_positions.borrow().get(id).copied())?
        .ok_or_else(|| format!("大小按钮未绘制：{id}"))?;
    let input = popup
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    input.click(point);
    settle(cx).await;
    Ok(())
}
async fn wheel_size(pin: WindowHandle<PinView>, delta: i16, cx: &mut AsyncApp) -> Result<()> {
    let point = inspect(pin, cx, |c| {
        c.ui_positions.borrow().get("width-menu").copied()
    })?
    .ok_or("大小滚轮按钮不存在")?;
    bars(pin, cx)?.2.wheel(point, delta)?;
    tick(cx).await;
    Ok(())
}
fn client_point(pin: WindowHandle<PinView>, point: Point, cx: &mut AsyncApp) -> Result<(u32, u32)> {
    pin.update(cx, |v, w, cx| -> Result<_> {
        let c = v.canvas.as_ref().ok_or("画布不存在")?.read(cx);
        let p = c.canvas_viewport(w).window_point(point);
        ensure(
            p.x >= 0. && p.y >= 0. && p.x <= 65535. && p.y <= 65535.,
            "专项输入超出client坐标范围",
        )?;
        Ok((p.x.round() as u32, p.y.round() as u32))
    })
    .map_err(|e| e.to_string())?
}
fn mouse(input: Input, message: u32, point: (u32, u32)) -> Result<()> {
    let wp = if message == WM_MOUSEMOVE {
        gpui_smoke::SMOKE_MOVE | 1
    } else if message == WM_LBUTTONDOWN {
        1
    } else {
        0
    };
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            message,
            WPARAM(wp),
            LPARAM(((point.1 << 16) | point.0) as isize),
        )?;
    }
    Ok(())
}
async fn frame(pin: WindowHandle<PinView>, source: Point, cx: &mut AsyncApp) -> Result<[u8; 4]> {
    let p = client_point(pin, source, cx)?;
    let image = frame_region(pin, [p.0, p.1, 1, 1], cx).await?;
    Ok(pixel(&image, 0, 0))
}
async fn frame_region(
    pin: WindowHandle<PinView>,
    region: [u32; 4],
    cx: &mut AsyncApp,
) -> Result<Image> {
    pin.update(cx, |_, w, _| w.request_frame_capture(region))
        .map_err(|e| e.to_string())??;
    for _ in 0..40 {
        tick(cx).await;
        if let Some(frame) = pin
            .update(cx, |_, w, _| w.take_frame_capture())
            .map_err(|e| e.to_string())?
        {
            let bytes = frame.map_err(|e| e.to_string())?;
            ensure(
                bytes.len() == region[2] as usize * region[3] as usize * 4,
                "GPU像素读回尺寸错误",
            )?;
            return Ok(Image {
                width: region[2],
                height: region[3],
                bgra: bytes,
                readback_ms: 0.,
            });
        }
    }
    Err("自有GPU帧未完成读回".into())
}
fn snapshot(
    pin: WindowHandle<PinView>,
    cx: &mut AsyncApp,
) -> Result<crate::pin_export::UserExportSnapshot> {
    pin.update(cx, |v, _, cx| v.user_export_snapshot(cx))
        .map_err(|e| e.to_string())?
}
async fn output(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Image> {
    let snapshot = snapshot(pin, cx)?;
    cx.background_executor()
        .spawn(async move { snapshot.render().map_err(|e| e.to_string()) })
        .await
        .map_err(Into::into)
}
fn action(pin: WindowHandle<PinView>, action: PinAction, cx: &mut AsyncApp) -> Result<()> {
    pin.update(cx, |v, w, cx| v.perform(action, w, cx))
        .map_err(|e| e.to_string().into())
}
async fn toolbar_and_stroke(
    pin: WindowHandle<PinView>,
    groups: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    settle(cx).await;
    for (id, tool) in [
        ("effect-mosaic", Tool::Mosaic),
        ("effect-blur", Tool::Blur),
        ("effect-local-fill", Tool::LocalFill),
    ] {
        property_click(pin, id, cx).await?;
        ensure(
            inspect(pin, cx, |c| c.editor.tool == tool)?,
            "实际分段点击没有切换下一笔效果",
        )?;
        ensure(
            inspect(pin, cx, |c| {
                c.ui_positions.borrow().contains_key("smart-effect")
            })?,
            "效果副栏缺少文字区域处理入口",
        )?;
    }
    property_click(pin, "effect-mosaic", cx).await?;
    ensure(
        inspect(pin, cx, |c| {
            c.editor.tool == Tool::Mosaic && !c.editor.effect_brush_mode
        })?,
        "矩形→填充→矩形丢失来源",
    )?;
    pin.update(cx, |v, w, cx| {
        v.annotation(Action::Tool(Tool::MosaicBrush), w, cx)
    })
    .map_err(|e| e.to_string())?;
    settle(cx).await;
    property_click(pin, "effect-local-fill", cx).await?;
    property_click(pin, "effect-mosaic", cx).await?;
    ensure(
        inspect(pin, cx, |c| {
            c.editor.tool == Tool::MosaicBrush && c.editor.effect_brush_mode
        })?,
        "笔刷→填充→笔刷丢失来源",
    )?;
    property_click(pin, "effect-local-fill", cx).await?;
    ensure(
        inspect(pin, cx, |c| {
            c.editor.tool == Tool::LocalFill && c.editor.style.width == 10.
        })?,
        "效果分段没有进入默认10px填充笔",
    )?;
    let before = bars(pin, cx)?;
    ensure(
        before.0.handle != before.2.handle,
        "一级二级不是独立原生窗口",
    )?;
    let state = pin
        .update(cx, |v, _, _| (v.state.zoom, v.state.opacity))
        .map_err(|e| e.to_string())?;
    wheel_size(pin, 120, cx).await?;
    ensure(
        inspect(pin, cx, |c| c.editor.style.width == 11.)?,
        "实际副栏滚轮不是1px",
    )?;
    size_preset(pin, "fill1", cx).await?;
    wheel_size(pin, -120, cx).await?;
    ensure(
        inspect(pin, cx, |c| c.editor.style.width == 1.)?,
        "实际滚轮越过1px下限",
    )?;
    size_preset(pin, "fill50", cx).await?;
    wheel_size(pin, 120, cx).await?;
    ensure(
        inspect(pin, cx, |c| c.editor.style.width == 50.)?,
        "实际滚轮越过50px上限",
    )?;
    size_preset(pin, "fill10", cx).await?;
    let after = bars(pin, cx)?;
    ensure(
        before.0.handle == after.0.handle
            && before.1 == after.1
            && before.2.handle == after.2.handle
            && before.3 == after.3,
        "填充大小操作重建或移动两级栏",
    )?;
    ensure(
        pin.update(cx, |v, _, _| (v.state.zoom, v.state.opacity))
            .map_err(|e| e.to_string())?
            == state,
        "大小滚轮误改贴图缩放/透明度",
    )?;
    groups.push(serde_json::json!({"group":"native-local-fill-effect-two-bars-width-wheel","ok":true,"primary_hwnd":before.0.handle,"secondary_hwnd":before.2.handle,"dimensions":[[before.1.width,before.1.height],[before.3.width,before.3.height]],"dpi":before.0.scale,"default_width":10,"step":1,"limits":[1,50],"same_hwnds_and_bounds":true,"pin_zoom_opacity_unchanged":true}));
    let input = main_input(pin, cx)?;
    let start = client_point(pin, Point { x: 65., y: 100. }, cx)?;
    let end = client_point(pin, Point { x: 105., y: 100. }, cx)?;
    mouse(input, WM_LBUTTONDOWN, start)?;
    mouse(input, WM_MOUSEMOVE, end)?;
    settle(cx).await;
    ensure(
        inspect(pin, cx, |c| {
            c.editor.local_fill_candidate().is_some()
                && c.editor.document.items().is_empty()
                && !c.editor.document.can_undo()
        })?,
        "蓝预览阶段提前提交/未建立真实填充草稿",
    )?;
    let blue = frame(pin, Point { x: 70., y: 100. }, cx).await?;
    ensure(
        blue[0] > 110 && blue[1] < 180 && blue[3] == 255,
        &format!("蓝色preview没有绘制：{blue:?}"),
    )?;
    mouse(input, WM_LBUTTONUP, end)?;
    settle(cx).await;
    ensure(
        inspect(pin, cx, |c| {
            c.editor.local_fill_candidate().is_none()
                && c.editor.document.items().len() == 1
                && c.editor.selected_ids().is_empty()
                && c.editor.document.items()[0].annotation.tool == Tool::LocalFill
        })?,
        "抬起没有一次提交填充/错误自动选中",
    )?;
    let committed = output(pin, cx).await?;
    ensure(pixel(&committed, 83, 100) == GREEN, "白字未按周围绿色填充")?;
    let gpu = frame(pin, Point { x: 83., y: 100. }, cx).await?;
    ensure(
        gpu.iter().zip(GREEN).all(|(&a, b)| a.abs_diff(b) <= 4),
        &format!("GPU填充与导出不一致：{gpu:?}"),
    )?;
    action(pin, PinAction::Undo, cx)?;
    settle(cx).await;
    let undone = output(pin, cx).await?;
    ensure(
        pixel(&undone, 83, 100) == [255; 4]
            && inspect(pin, cx, |c| {
                c.editor.document.items().is_empty()
                    && !c.editor.document.can_undo()
                    && c.editor.document.can_redo()
            })?,
        "一次撤销没有精确回到原始白字",
    )?;
    action(pin, PinAction::Redo, cx)?;
    settle(cx).await;
    let redone = output(pin, cx).await?;
    ensure(redone.bgra == committed.bgra, "一次重做填充像素不一致")?;
    crate::export::save_png(redone, &Path::new(FOLDER).join("stroke-export.png"))?;
    groups.push(serde_json::json!({"group":"native-local-fill-stroke-preview-commit-history-gpu-export","ok":true,"posted_input":true,"preview_bgra":blue,"committed_gpu_bgra":gpu,"export_center":GREEN,"draft_uncommitted":true,"auto_selected":false,"undo_commands":1,"redo_pixels_exact":true,"gpu_backend":"own framebuffer readback","export_route":"production user_export_snapshot background render"}));
    Ok(())
}
// 独立整数采样，不复用 OutputProjection、projected Scene 或 GPU tile 坐标实现。
fn nearest_pixel(source: &Image, size: (u32, u32), x: u32, y: u32) -> [u8; 4] {
    let sx = ((u64::from(x) * 2 + 1) * u64::from(source.width) / (u64::from(size.0) * 2)) as u32;
    let sy = ((u64::from(y) * 2 + 1) * u64::from(source.height) / (u64::from(size.1) * 2)) as u32;
    pixel(source, sx.min(source.width - 1), sy.min(source.height - 1))
}
async fn fractional_tile_pixels(
    groups: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let pin = open_owned(fixture(2200, 64, false), (80, 180), cx)?;
    settle(cx).await;
    let patches = pin
        .update(cx, |v, _, cx| -> Result<_> {
            v.canvas.as_ref().unwrap().update(cx, |c, cx| -> Result<_> {
                // 底图是平色；白色矢量带内部没有底图 LINEAR 或轮廓 AA 误差。
                // 接缝旁上下交替的一像素细线让 mask 的实/透明像素相邻，能暴露 LINEAR 污色。
                let mut stripe = solid_rect(
                    Point { x: 8., y: 31. },
                    Point { x: 2185., y: 33. },
                    0xffffff,
                );
                stripe.style.width = 0.25;
                c.editor.document.insert(stripe);
                c.editor.set_width(1.);
                c.editor.style.pencil_smooth = false;
                c.editor
                    .pointer_down_at(Point { x: 10.5, y: 31.5 }, 0., false);
                c.editor.pointer_move_at(Point { x: 2047.5, y: 31.5 });
                for x in 2048..2068 {
                    c.editor.pointer_move_at(Point {
                        x: x as f32 + 0.5,
                        y: if x % 2 == 0 { 26.5 } else { 32.5 },
                    });
                }
                c.editor.pointer_move_at(Point { x: 2180.5, y: 31.5 });
                ensure(
                    c.editor.local_fill_candidate().is_some(),
                    "分块夹具没有真实Editor填充候选",
                )?;
                c.try_finish_local_fill()?;
                ensure(
                    c.editor.local_fill_candidate().is_none()
                        && c.editor.document.items().len() == 2,
                    "分块填充未通过生产准备门提交",
                )?;
                let scene = c.prepared_scene();
                let patches = scene
                    .layers
                    .iter()
                    .filter_map(|layer| match layer {
                        crate::scene::Layer::LocalFill { patches } => Some(patches.to_vec()),
                        _ => None,
                    })
                    .flatten()
                    .collect::<Vec<_>>();
                cx.notify();
                Ok(patches)
            })
        })
        .map_err(|e| e.to_string())??;
    ensure(
        patches.len() == 2
            && patches[0].rect.width == 2048
            && patches[0].rect.x + patches[0].rect.width == patches[1].rect.x,
        "夹具未跨真实2048px FillPatch边界",
    )?;
    ensure(
        patches.iter().all(|p| {
            p.image.bgra.chunks_exact(4).any(|b| b[3] == 0)
                && p.image.bgra.chunks_exact(4).any(|b| b[3] == 255)
        }),
        "分块夹具没有透明mask边",
    )?;
    settle(cx).await;
    let baseline = output(pin, cx).await?;
    let seam = patches[1].rect.x;
    let mut transitions = 0;
    for y in 31..33 {
        for x in seam - 8..seam + 8 {
            let a = pixel(&baseline, x, y);
            let b = pixel(&baseline, x + 1, y);
            if (a == GREEN && b == [255; 4]) || (a == [255; 4] && b == GREEN) {
                transitions += 1;
            }
        }
    }
    ensure(transitions >= 4, "接缝旁未建立相邻单像素实/透明pattern")?;
    action(pin, PinAction::Zoom(1.5), cx)?;
    settle(cx).await;
    let viewport = pin
        .update(cx, |v, w, cx| {
            let size = w.viewport_size();
            let projection = v.canvas.as_ref().unwrap().read(cx).canvas_viewport(w);
            (
                projection,
                (
                    (f32::from(size.width) * w.scale_factor()).round() as u32,
                    (f32::from(size.height) * w.scale_factor()).round() as u32,
                ),
            )
        })
        .map_err(|e| e.to_string())?;
    ensure(
        (viewport.0.zoom - 1.5).abs() < 0.00001
            && viewport.0.offset.x.abs() < 0.00001
            && viewport.0.offset.y.abs() < 0.00001
            && viewport.1 == (3300, 96),
        "真实Pin客户区没有150%无留边投影",
    )?;
    let origin = (
        patches[0].rect.x as f32 * 1.5,
        patches[0].rect.y as f32 * 1.5,
    );
    let seam_physical = seam as f32 * 1.5;
    ensure(
        origin.0.fract().abs() == 0.5
            && origin.1.fract().abs() == 0.5
            && seam_physical.fract().abs() == 0.5,
        "夹具未触发小数origin和tile接缝",
    )?;
    let enlarged = output(pin, cx).await?;
    ensure(
        (enlarged.width, enlarged.height) == viewport.1,
        "150%生产导出与客户区尺寸不同",
    )?;
    // 白色矢量带内部的这两行全覆盖。普通矢量轮廓 AA 保留其自身合同，不纳入 nearest 比较。
    let region = [seam_physical.floor() as u32 - 24, 47, 52, 2];
    let gpu = frame_region(pin, region, cx).await?;
    let mut checked = 0;
    let mut colors = [0_u32; 2];
    for y in 0..region[3] {
        for x in 0..region[2] {
            let (ox, oy) = (region[0] + x, region[1] + y);
            let expected = nearest_pixel(&baseline, viewport.1, ox, oy);
            ensure(
                expected == GREEN || expected == [255; 4],
                "oracle采样落在了下层矢量外沿AA而非带内部",
            )?;
            colors[usize::from(expected == [255; 4])] += 1;
            ensure(
                pixel(&enlarged, ox, oy) == expected,
                &format!("生产Nearest导出错误({ox},{oy})"),
            )?;
            let actual = pixel(&gpu, x, y);
            ensure(
                actual
                    .iter()
                    .zip(expected)
                    .all(|(&a, b)| a.abs_diff(b) <= 3),
                &format!("150% GPU透明边/tile相位错误({ox},{oy}):{actual:?}!={expected:?}"),
            )?;
            checked += 1;
        }
    }
    ensure(
        colors[0] > 8 && colors[1] > 8,
        "GPU检查没有同时覆盖实色和透明mask像素",
    )?;
    crate::export::save_png(gpu, &Path::new(FOLDER).join("fractional-tile-gpu.png"))?;
    crate::export::save_png(
        enlarged,
        &Path::new(FOLDER).join("fractional-tile-export.png"),
    )?;
    groups.push(serde_json::json!({"group":"native-local-fill-fractional-tile-nearest-gpu-export","ok":true,"zoom":1.5,"source_dimensions":[2200,64],"output_dimensions":viewport.1,"patch_rects":patches.iter().map(|p|[p.rect.x,p.rect.y,p.rect.width,p.rect.height]).collect::<Vec<_>>(),"physical_patch_origin":origin,"physical_tile_boundary":seam_physical,"source_adjacent_transitions":transitions,"readback_region":region,"checked_pixels":checked,"filled_and_transparent_pixels":colors,"channel_tolerance":3,"oracle":"independent integer center Nearest of production 1x snapshot; no shared OutputProjection/projected Scene/GPU tile arithmetic","scope":"flat lower-vector interior, fill transparent edges and both sides of actual 2048px split; unrelated vector contour AA excluded","fixture_producer":"Editor gesture -> CaptureView::try_finish_local_fill -> production user_export_snapshot","gpu_backend":"real owned-window framebuffer readback"}));
    pin.update(cx, |v, w, cx| {
        v.close_toolbar(cx);
        w.remove_window();
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}
async fn smart_button(
    pin: WindowHandle<PinView>,
    groups: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    // 观察真实 CaptureView 通知。这里既不直接调用 Action，也不注入 processing 状态。
    let observed = Rc::new(RefCell::new((false, false, String::new())));
    let probe = observed.clone();
    let subscription = pin
        .update(cx, |v, _, cx| {
            cx.observe(v.canvas.as_ref().unwrap(), move |_, canvas, cx| {
                let c = canvas.read(cx);
                let mut probe = probe.borrow_mut();
                if c.processing {
                    probe.0 = true;
                }
                if probe.0 && !c.processing {
                    probe.1 = true;
                    probe.2 = c.status.clone();
                }
            })
        })
        .map_err(|e| e.to_string())?;
    ensure(
        inspect(pin, cx, |c| !c.processing && c.recognition_result.is_none())?,
        "真实按钮检查前已有识别请求",
    )?;
    let point = inspect(pin, cx, |c| {
        c.ui_positions.borrow().get("smart-effect").copied()
    })?
    .ok_or("未绘制文字区域处理按钮")?;
    let secondary = bars(pin, cx)?.2;
    secondary.click(point);
    for _ in 0..250 {
        tick(cx).await;
        if observed.borrow().1 {
            break;
        }
    }
    let (started, finished, status) = observed.borrow().clone();
    drop(subscription);
    ensure(
        started && finished,
        "真实文字区域按钮未启动并完成隔离的本机后台请求",
    )?;
    ensure(
        inspect(pin, cx, |c| {
            c.editor.tool == Tool::LocalFill
                && c.result_title == "本机文字识别"
                && c.recognition_result.is_none()
        })?,
        "文字区域按钮未保留填充模式/错误进入纯OCR结果",
    )?;
    groups.push(serde_json::json!({"group":"native-local-fill-smart-effect-production-button","ok":true,"probe":"smart-effect","posted_input":true,"secondary_hwnd":secondary.handle,"capture_processing_started":started,"capture_processing_finished":finished,"terminal_status":status,"frozen_effect":"localFill","local_ocr_engine_attempted":true,"ocr_accuracy_is_not_asserted":true,"source":"owned synthetic Pin only; deterministic ROI pixels are checked separately through the same production callback API"}));
    Ok(())
}
fn open_owned(
    image: Image,
    origin: (i32, i32),
    cx: &mut AsyncApp,
) -> Result<WindowHandle<PinView>> {
    let pin = cx.update(|cx| PinView::open_state(image, origin, PinState::default(), cx))??;
    pin.update(cx, |v, w, cx| -> Result<()> {
        gpui_smoke::isolate_input(hwnd(w))?;
        v.ensure_canvas(w, cx)?;
        v.perform(PinAction::Chrome, w, cx);
        v.annotation(Action::Tool(Tool::LocalFill), w, cx);
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    Ok(pin)
}

fn solid_rect(from: Point, to: Point, color: u32) -> Annotation {
    let mut annotation = Annotation::new(
        Tool::Rectangle,
        from,
        Style {
            filled: true,
            color,
            ..Default::default()
        },
    );
    annotation.update(to);
    annotation
}
async fn batch_fill(groups: &mut Vec<serde_json::Value>, cx: &mut AsyncApp) -> Result<()> {
    let pin = open_owned(fixture(512, 256, true), (100, 430), cx)?;
    settle(cx).await;
    pin.update(cx, |v, _, cx| {
        v.canvas.as_ref().unwrap().update(cx, |c, cx| {
            // 下层黄色四边环保留内部白字，证明取色包含下层标注。
            for (x, y, w, h) in [
                (70., 92., 30., 5.),
                (70., 103., 30., 5.),
                (70., 97., 5., 6.),
                (95., 97., 5., 6.),
            ] {
                c.editor.document.insert(solid_rect(
                    Point { x, y },
                    Point { x: x + w, y: y + h },
                    0xc89628,
                ));
            }
            cx.notify();
        });
    })
    .map_err(|e| e.to_string())?;
    let request = pin
        .update(cx, |v, _, cx| {
            v.canvas
                .as_ref()
                .unwrap()
                .read(cx)
                .smart_effect_request(Rect::full(512, 256))
        })
        .map_err(|e| e.to_string())?;
    let count = pin
        .update(cx, |v, _, cx| {
            v.canvas.as_ref().unwrap().update(cx, |c, cx| {
                let result = c.apply_smart_effect(
                    &request,
                    vec![
                        Rect {
                            x: 75,
                            y: 97,
                            width: 20,
                            height: 6,
                        },
                        Rect {
                            x: 395,
                            y: 97,
                            width: 20,
                            height: 6,
                        },
                    ],
                );
                cx.notify();
                result
            })
        })
        .map_err(|e| e.to_string())??;
    ensure(count == 2, "冻结批量入口没有处理两个ROI")?;
    let full = output(pin, cx).await?;
    let yellow = [40, 150, 200, 255];
    ensure(
        pixel(&full, 83, 100) == yellow && pixel(&full, 403, 100) == PURPLE,
        "批量ROI未独立采样各自下层颜色",
    )?;
    ensure(
        inspect(pin, cx, |c| {
            c.editor.document.items().len() == 5 && c.editor.selected_ids().is_empty()
        })?,
        "两个ROI未形成一次提交/自动选中",
    )?;
    action(pin, PinAction::Undo, cx)?;
    let undone = output(pin, cx).await?;
    ensure(
        pixel(&undone, 83, 100) == [255; 4]
            && pixel(&undone, 403, 100) == [255; 4]
            && inspect(pin, cx, |c| c.editor.document.items().len() == 4)?,
        "一次撤销没有同时移除两个ROI",
    )?;
    action(pin, PinAction::Redo, cx)?;
    ensure(
        output(pin, cx).await?.bgra == full.bgra,
        "批量重做未精确恢复",
    )?;
    let previous = pin
        .update(cx, |v, _, cx| {
            v.canvas.as_ref().unwrap().update(cx, |c, _| {
                let scene = c.prepared_scene();
                scene
                    .layers
                    .iter()
                    .filter_map(|layer| match layer {
                        crate::scene::Layer::LocalFill { patches } => {
                            Some(patches.iter().map(|p| p.image.clone()).collect::<Vec<_>>())
                        }
                        _ => None,
                    })
                    .flatten()
                    .collect::<Vec<_>>()
            })
        })
        .map_err(|e| e.to_string())?;
    ensure(previous.len() >= 2, "批量填充缺少独立局部结果分块")?;
    let unchanged = pin
        .update(cx, |v, _, cx| {
            v.canvas.as_ref().unwrap().update(cx, |c, cx| {
                // 上层只改第一孔的取色环，不盖孔本身；旧填充色必须继续使用原基线。
                c.editor.document.insert(solid_rect(
                    Point { x: 70., y: 95. },
                    Point { x: 100., y: 97. },
                    0x3285fa,
                ));
                let scene = c.prepared_scene();
                let next = scene
                    .layers
                    .iter()
                    .filter_map(|layer| match layer {
                        crate::scene::Layer::LocalFill { patches } => {
                            Some(patches.iter().map(|p| p.image.clone()).collect::<Vec<_>>())
                        }
                        _ => None,
                    })
                    .flatten()
                    .collect::<Vec<_>>();
                cx.notify();
                previous.len() == next.len()
                    && previous.iter().zip(next).all(|(a, b)| Arc::ptr_eq(a, &b))
            })
        })
        .map_err(|e| e.to_string())?;
    let upper = output(pin, cx).await?;
    ensure(
        unchanged && pixel(&upper, 83, 100) == yellow && pixel(&upper, 403, 100) == PURPLE,
        "上层变化污染了已提交填充基线/缓存",
    )?;
    crate::export::save_png(upper, &Path::new(FOLDER).join("batch-export.png"))?;
    let before_stale = failure_state(pin, cx)?;
    let stale_document_rejected = pin
        .update(cx, |v, _, cx| {
            v.canvas.as_ref().unwrap().update(cx, |c, _| {
                c.apply_smart_effect(
                    &request,
                    vec![Rect {
                        x: 230,
                        y: 60,
                        width: 12,
                        height: 8,
                    }],
                )
                .is_err()
            })
        })
        .map_err(|e| e.to_string())?;
    ensure(
        stale_document_rejected && failure_state(pin, cx)? == before_stale,
        "过期文档请求未原子拒绝/改变items、revision、redo或预设",
    )?;
    let blur_style = Style {
        block_size: 17,
        ..Default::default()
    };
    let selected_id = pin
        .update(cx, |v, w, cx| {
            let id = v.canvas.as_ref().unwrap().update(cx, |c, cx| {
                let mut blur = Annotation::new(Tool::Blur, Point { x: 200., y: 50. }, blur_style);
                blur.update(Point { x: 260., y: 90. });
                let id = c.editor.document.insert(blur);
                cx.notify();
                id
            });
            v.annotation(Action::Tool(Tool::Edit), w, cx);
            id
        })
        .map_err(|e| e.to_string())?;
    settle(cx).await;
    let selected_point = client_point(pin, Point { x: 230., y: 70. }, cx)?;
    main_input(pin, cx)?.drag(selected_point, selected_point);
    settle(cx).await;
    ensure(
        inspect(pin, cx, |c| {
            c.editor.tool == Tool::Edit && c.editor.selected_ids() == vec![selected_id]
        })?,
        "真实Edit点击没有选中Blur对象",
    )?;
    let selected_request = pin
        .update(cx, |v, _, cx| {
            v.canvas
                .as_ref()
                .unwrap()
                .read(cx)
                .smart_effect_request(Rect::full(512, 256))
        })
        .map_err(|e| e.to_string())?;
    let selected_count = pin
        .update(cx, |v, _, cx| {
            v.canvas.as_ref().unwrap().update(cx, |c, cx| {
                let result = c.apply_smart_effect(
                    &selected_request,
                    vec![Rect {
                        x: 230,
                        y: 60,
                        width: 12,
                        height: 8,
                    }],
                );
                cx.notify();
                result
            })
        })
        .map_err(|e| e.to_string())??;
    ensure(
        selected_count == 1
            && inspect(pin, cx, |c| {
                c.editor.document.items().last().is_some_and(|o| {
                    o.annotation.tool == Tool::Blur && o.annotation.style == blur_style
                })
            })?,
        "Smart请求未冻结选中Blur上下文/selectedStyle",
    )?;
    let source_request = pin
        .update(cx, |v, _, cx| {
            v.canvas.as_ref().unwrap().update(cx, |c, _| {
                let request = c.smart_effect_request(Rect::full(512, 256));
                c.source.bounds.width += 1;
                request
            })
        })
        .map_err(|e| e.to_string())?;
    let before_source = failure_state(pin, cx)?;
    let stale_source_rejected = pin
        .update(cx, |v, _, cx| {
            v.canvas.as_ref().unwrap().update(cx, |c, _| {
                c.apply_smart_effect(
                    &source_request,
                    vec![Rect {
                        x: 230,
                        y: 60,
                        width: 12,
                        height: 8,
                    }],
                )
                .is_err()
            })
        })
        .map_err(|e| e.to_string())?;
    ensure(
        stale_source_rejected && failure_state(pin, cx)? == before_source,
        "过期源身份请求未原子拒绝/改变items、revision、redo或预设",
    )?;
    pin.update(cx, |v, _, cx| {
        v.canvas
            .as_ref()
            .unwrap()
            .update(cx, |c, _| c.source.bounds.width -= 1)
    })
    .map_err(|e| e.to_string())?;
    groups.push(serde_json::json!({"group":"native-local-fill-batch-independent-roi-frozen-baseline","ok":true,"roi_count":count,"left_bgra":yellow,"right_bgra":PURPLE,"samples_lower_annotations":true,"one_undo_for_two_regions":true,"redo_pixels_exact":true,"upper_ring_change_keeps_patch_identity":true,"selected_context_posted_click":true,"selected_context_tool":"Blur","selected_context_block_size":17,"selected_style_frozen":true,"stale_document_rejected_atomically":stale_document_rejected,"stale_source_rejected_atomically":stale_source_rejected,"producer":"production smart_effect_request/apply_smart_effect shared with OCR callback","ocr_engine_invoked":false}));
    pin.update(cx, |v, w, cx| {
        v.close_toolbar(cx);
        w.remove_window();
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}
#[derive(Debug, PartialEq)]
struct FailureState {
    draft: Option<Annotation>,
    objects: Vec<crate::document::Object>,
    revision: u64,
    edit_revision: u64,
    redo: bool,
    tool: Tool,
    effect_brush_mode: bool,
    style: Style,
    styles: Vec<(Tool, Style)>,
    chrome: bool,
    editing: bool,
    toolbar: Option<usize>,
    secondary: Option<usize>,
    popup: Option<usize>,
    panel: crate::toolbar_layout::ToolbarPanel,
    saving: bool,
    processing: bool,
    processing_redo: usize,
    preset_file: Option<Vec<u8>>,
    output_files: Vec<(std::path::PathBuf, u64, std::time::SystemTime)>,
}
fn output_files() -> Result<Vec<(std::path::PathBuf, u64, std::time::SystemTime)>> {
    fn collect(
        path: &Path,
        result: &mut Vec<(std::path::PathBuf, u64, std::time::SystemTime)>,
    ) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                collect(&entry.path(), result)?;
            } else {
                result.push((entry.path(), metadata.len(), metadata.modified()?));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    collect(&Path::new(FOLDER).join("output"), &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}
fn failure_state(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<FailureState> {
    pin.update(cx, |v, _, cx| -> Result<_> {
        let mut state = {
            let c = v.canvas.as_ref().ok_or("失败专项画布不存在")?.read(cx);
            let mut styles = c.editor.tool_styles().collect::<Vec<_>>();
            styles.sort_by_key(|(tool, _)| crate::toolbar_config::tool_id(*tool).unwrap_or(""));
            FailureState {
                draft: c.editor.local_fill_candidate(),
                objects: c.editor.document.items().to_vec(),
                revision: c.editor.document.revision(),
                edit_revision: c.editor.document.edit_revision(),
                redo: c.editor.document.can_redo(),
                tool: c.editor.tool,
                effect_brush_mode: c.editor.effect_brush_mode,
                style: c.editor.style,
                styles,
                chrome: v.chrome,
                editing: c.pin_editing,
                toolbar: None,
                secondary: None,
                popup: None,
                panel: c.toolbar_panel,
                saving: v.saving,
                processing: v.processing,
                processing_redo: v.redo_edits.len(),
                preset_file: std::fs::read(crate::preferences::data_dir().join("tool-presets.json"))
                    .ok(),
                output_files: output_files()?,
            }
        };
        let children = v
            .toolbar
            .map(|bar| {
                bar.update(cx, |bar, w, cx| {
                    (hwnd(w).0 as usize, bar.properties, bar.popup(cx))
                })
            })
            .transpose()
            .map_err(|e| e.to_string())?;
        if let Some((primary, secondary, popup)) = children {
            state.toolbar = Some(primary);
            state.secondary = secondary
                .map(|bar| bar.update(cx, |_, w, _| hwnd(w).0 as usize))
                .transpose()
                .map_err(|e| e.to_string())?;
            state.popup = popup
                .map(|popup| popup.update(cx, |_, w, _| hwnd(w).0 as usize))
                .transpose()
                .map_err(|e| e.to_string())?;
        }
        Ok(state)
    })
    .map_err(|e| e.to_string())?
}
async fn failure_gates(
    budget: bool,
    groups: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let side = if budget { 64 } else { 32 };
    let pin = open_owned(fixture(side, side, false), (680, 100), cx)?;
    settle(cx).await;
    pin.update(cx, |v, _, cx| {
        v.canvas.as_ref().unwrap().update(cx, |c, cx| {
            let mut dummy =
                Annotation::new(Tool::Rectangle, Point { x: 4., y: 4. }, Style::default());
            dummy.update(Point { x: 10., y: 10. });
            c.editor.document.insert(dummy);
            c.editor.document.undo();
            c.editor.style.width = 50.;
            if budget {
                c.source.bounds.width = 8000;
                c.source.bounds.height = 8000;
                c.editor.canvas = Rect::full(8000, 8000);
            }
            cx.notify();
        });
    })
    .map_err(|e| e.to_string())?;
    // 等真实文档 observe 消费种子编辑，再建立独立的图像处理 redo 边界。
    settle(cx).await;
    pin.update(cx, |v, _, _| {
        let checkpoint = v.snapshot();
        v.redo_edits.push(checkpoint);
    })
    .map_err(|e| e.to_string())?;
    let input = main_input(pin, cx)?;
    let (a, b) = if budget {
        // client0逆投影仍在原64px crop内。保持crop，保证输出失败来自真实Fill准备门。
        (Point { x: 1., y: 1. }, Point { x: 7875., y: 7875. })
    } else {
        (Point { x: 2., y: 16. }, Point { x: 30., y: 16. })
    };
    let start = client_point(pin, a, cx)?;
    let end = client_point(pin, b, cx)?;
    mouse(input, WM_LBUTTONDOWN, start)?;
    mouse(input, WM_MOUSEMOVE, end)?;
    mouse(input, WM_LBUTTONUP, end)?;
    settle(cx).await;
    let before = failure_state(pin, cx)?;
    ensure(
        before.draft.is_some()
            && before.objects.is_empty()
            && before.redo
            && before.processing_redo == 1,
        "失败夹具未保留草稿/redo或提前提交",
    )?;
    let error = inspect(pin, cx, |c| c.status.clone())?;
    ensure(
        if budget {
            error.contains("预算")
        } else {
            error.contains("背景颜色")
        },
        &format!("未触发指定失败原因：{error}"),
    )?;
    pin.update(cx, |v, w, cx| {
        v.annotation(Action::Tool(Tool::Rectangle), w, cx)
    })
    .map_err(|e| e.to_string())?;
    ensure(
        failure_state(pin, cx)? == before,
        "失败门切换工具消费草稿/redo/presets",
    )?;
    ensure(snapshot(pin, cx).is_err(), "失败门仍生成用户输出snapshot")?;
    ensure(
        failure_state(pin, cx)? == before,
        "失败snapshot改变事务状态",
    )?;
    action(pin, PinAction::Save, cx)?;
    settle(cx).await;
    ensure(
        failure_state(pin, cx)? == before,
        "失败门Save开启面板/输出或改变事务状态",
    )?;
    action(pin, PinAction::QuickSave, cx)?;
    settle(cx).await;
    ensure(
        failure_state(pin, cx)? == before,
        "失败门QuickSave改状态/开始输出",
    )?;
    action(pin, PinAction::Chrome, cx)?;
    settle(cx).await;
    ensure(failure_state(pin, cx)? == before, "失败门仍收起工具栏")?;
    action(pin, PinAction::Close, cx)?;
    settle(cx).await;
    ensure(
        unsafe { IsWindow(Some(HWND(input.handle as *mut _))) }.as_bool()
            && failure_state(pin, cx)? == before,
        "失败门仍关闭窗口/丢失事务",
    )?;
    let secondary = before.secondary.ok_or("失败夹具二级窗口不存在")?;
    for (name, handle) in [
        ("secondary", secondary),
        ("primary", before.toolbar.ok_or("失败夹具一级窗口不存在")?),
    ] {
        unsafe {
            PostMessageW(Some(HWND(handle as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0))?;
        }
        settle(cx).await;
        ensure(
            unsafe { IsWindow(Some(HWND(handle as *mut _))) }.as_bool()
                && failure_state(pin, cx)? == before,
            &format!("失败{name} WM_CLOSE仍收栏/关闭children或破坏事务"),
        )?;
    }
    pin.update(cx, |v, _, cx| -> Result<()> {
        if let Some(canvas) = &v.canvas {
            canvas.update(cx, |c, _| -> Result<()> {
                c.editor.cancel_gesture();
                c.editor.redo();
                ensure(
                    c.editor.document.items().len() == 1
                        && c.editor.document.items()[0].annotation.tool == Tool::Rectangle,
                    "失败后取消草稿没有保留可恢复的原始redo命令",
                )?;
                if budget {
                    c.source.bounds.width = 64;
                    c.source.bounds.height = 64;
                    c.editor.canvas = Rect::full(64, 64);
                }
                Ok(())
            })?;
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    unsafe {
        PostMessageW(
            Some(HWND(secondary as *mut _)),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        )?;
    }
    settle(cx).await;
    ensure(
        !unsafe { IsWindow(Some(HWND(secondary as *mut _))) }.as_bool()
            && pin
                .update(cx, |v, _, _| !v.chrome && v.toolbar.is_none())
                .map_err(|e| e.to_string())?,
        "取消失败草稿后原生二级WM_CLOSE未正常收栏",
    )?;
    groups.push(serde_json::json!({"group":if budget{"native-local-fill-budget-failure-transaction-gates"}else{"native-local-fill-zero-neighborhood-failure-transaction-gates"},"ok":true,"reason":error,"posted_mouse_up_failed":true,"draft_items":before.draft.as_ref().map(|a|a.points.len()),"redo_preserved_and_restored_after_cancel":true,"processing_redo_preserved":true,"preset_file_unchanged":true,"output_files_unchanged":true,"secondary_hwnd":secondary,"failed_native_close_preserves_layer_group":true,"cancel_then_secondary_close_succeeds":true,"gates":["Action::Tool","user_export_snapshot","PinAction::Save","PinAction::QuickSave","PinAction::Chrome","PinAction::Close","secondary WM_CLOSE","primary WM_CLOSE"],"synthetic_bounds":if budget{serde_json::json!([8000,8000])}else{serde_json::json!([32,32])},"allocated_source_dimensions":[side,side],"budget_case_is_metadata_precheck_only":budget}));
    pin.update(cx, |v, w, cx| {
        v.close_toolbar(cx);
        w.remove_window();
    })
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    *errors.borrow_mut() = Some("LocalFill专项未完成".into());
    let setup = (|| -> Result<_> {
        std::fs::create_dir_all(FOLDER)?;
        ensure(
            crate::preferences::data_dir()
                == std::env::current_dir()?.join(FOLDER).join("settings"),
            "LocalFill设置未隔离",
        )?;
        crate::preferences::Preferences {
            history: false,
            pin_restore: false,
            ..Default::default()
        }
        .save()?;
        std::fs::write(
            crate::preferences::data_dir().join("tool-presets.json"),
            "{}\n",
        )?;
        let mut store = crate::save_store::SaveStore::load();
        store.config = crate::save_options::SaveConfig::default();
        store.config.directory = std::env::current_dir()?.join(FOLDER).join("output");
        store.save()?;
        let pin =
            PinView::open_state(fixture(512, 256, true), (100, 100), PinState::default(), cx)?;
        pin.update(cx, |v, w, cx| -> Result<()> {
            gpui_smoke::isolate_input(hwnd(w))?;
            v.ensure_canvas(w, cx)?;
            v.perform(PinAction::Chrome, w, cx);
            v.annotation(Action::Tool(Tool::Mosaic), w, cx);
            Ok(())
        })??;
        Ok(pin)
    })();
    let pin = match setup {
        Ok(pin) => pin,
        Err(error) => {
            *errors.borrow_mut() = Some(error.to_string());
            cx.quit();
            return;
        }
    };
    cx.spawn(async move|cx|{
        let mut groups=Vec::new();
        let result=async {
            toolbar_and_stroke(pin,&mut groups,cx).await?;
            smart_button(pin,&mut groups,cx).await?;
            batch_fill(&mut groups,cx).await?;
            fractional_tile_pixels(&mut groups,cx).await?;
            failure_gates(false,&mut groups,cx).await?;
            failure_gates(true,&mut groups,cx).await?;
            Ok::<(),Box<dyn std::error::Error>>(())
        }.await;
        let error=result.err().map(|e|e.to_string());
        let report=serde_json::json!({"test":"local-fill-native","ok":error.is_none(),"error":error,"groups":groups,"clipboard_read":false,"clipboard_written":false,"desktop_read":false,"global_pointer_moved":false,"global_input_injected":false,"source":"owned synthetic Pin/two-toolbar HWNDs/posted input/GPU framebuffer/production export snapshot"});
        let write=std::fs::write(Path::new(FOLDER).join("report.json"),serde_json::to_vec_pretty(&report).unwrap_or_default());
        *errors.borrow_mut()=error.or_else(||write.err().map(|e|e.to_string()));
        println!("{report}");
        let _=pin.update(cx,|v,w,cx|{if let Some(c)=&v.canvas{c.update(cx,|c,_|{c.editor.cancel_gesture();});}v.close_toolbar(cx);w.remove_window();});
        let _=cx.update(|cx|cx.quit());
    }).detach();
}
