//! 多选圆角原生输入与填充矩形导出验收，仅使用合成白底和本次自有 HWND。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture_view::CaptureView,
    document::{Object, ObjectId},
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_model::Placement,
    pin_property_bar::PinPropertyBar,
    pin_toolbar::PinToolbar,
    pin_view::PinView,
    spotlight_smoke::frame,
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{AsyncApp, WeakEntity, WindowHandle};
use std::collections::HashMap;
use windows::Win32::{Foundation::HWND, UI::WindowsAndMessaging::*};

#[derive(Clone, Debug, PartialEq)]
struct State {
    objects: Vec<Object>,
    revision: u64,
    edit_revision: u64,
    redo: bool,
    selected: Option<ObjectId>,
    selection: Vec<ObjectId>,
    crop: Option<Rect>,
    tool: Tool,
    style: Style,
    presets: HashMap<Tool, Style>,
}
fn read(v: &CaptureView) -> State {
    State {
        objects: v.editor.document.items().to_vec(),
        revision: v.editor.document.revision(),
        edit_revision: v.editor.document.edit_revision(),
        redo: v.editor.document.can_redo(),
        selected: v.editor.selected,
        selection: v.editor.selection.clone(),
        crop: v.editor.crop,
        tool: v.editor.tool,
        style: v.editor.style,
        presets: v.editor.tool_styles().collect(),
    }
}
async fn state(v: &WeakEntity<CaptureView>, cx: &mut AsyncApp) -> Result<State> {
    v.update(cx, |v, _| read(v))
        .map_err(|e| e.to_string().into())
}
fn pass(name: &str) {
    println!("{{\"test\":\"{name}\",\"ok\":true}}");
}
fn visible(input: Input) {
    unsafe {
        let h = HWND(input.handle as *mut _);
        if !IsWindowVisible(h).as_bool() {
            let _ = ShowWindow(h, SW_SHOW);
        }
    }
}
async fn position(
    v: &WeakEntity<CaptureView>,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<(f32, f32)> {
    for _ in 0..15 {
        let p = v
            .update(cx, |v, _| v.ui_positions.borrow().get(id).copied())
            .map_err(|e| e.to_string())?;
        if let Some(p) = p {
            return Ok(p);
        }
        frame(cx).await;
    }
    Err(format!("多选圆角控件未绘制：{id}").into())
}
async fn click(
    v: &WeakEntity<CaptureView>,
    input: Input,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    input.click(position(v, id, cx).await?);
    frame(cx).await;
    Ok(())
}
async fn panel(
    v: &WeakEntity<CaptureView>,
    expected: ToolbarPanel,
    cx: &mut AsyncApp,
) -> Result<()> {
    let actual = v
        .update(cx, |v, _| v.toolbar_panel)
        .map_err(|e| e.to_string())?;
    if actual != expected {
        return Err(format!("多选圆角页状态错误：{actual:?}，应为{expected:?}").into());
    }
    Ok(())
}
fn digits(input: Input, number: &str) -> Result<()> {
    // KEYDOWN 经过实际快捷键路由和 TranslateMessage，不直接注入 WM_CHAR。
    for _ in 0..16 {
        input.key(8);
    }
    for digit in number.bytes() {
        if !digit.is_ascii_digit() {
            return Err("圆角专项仅使用数字键".into());
        }
        input.key(digit as usize);
    }
    Ok(())
}
#[derive(Clone, Copy)]
struct Controls {
    entry: Input,
    properties: Option<WindowHandle<PinPropertyBar>>,
}
async fn open(v: &WeakEntity<CaptureView>, controls: Controls, cx: &mut AsyncApp) -> Result<Input> {
    click(v, controls.entry, "radius-menu", cx).await?;
    panel(v, ToolbarPanel::Radius, cx).await?;
    let input = if let Some(properties) = controls.properties {
        let mut input = None;
        for _ in 0..15 {
            let popup = properties
                .update(cx, |v, _, _| v.popup)
                .map_err(|e| e.to_string())?;
            input = popup.and_then(|popup| {
                popup
                    .update(cx, |_, w, _| Input {
                        handle: hwnd(w).0 as usize,
                        scale: w.scale_factor(),
                    })
                    .ok()
            });
            if input.is_some() {
                break;
            }
            frame(cx).await;
        }
        input.ok_or("贴图多选圆角参数窗未创建")?
    } else {
        controls.entry
    };
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    visible(input);
    frame(cx).await;
    position(v, "parameter-value", cx).await?;
    Ok(input)
}
fn seed(v: &mut CaptureView, area: Rect) {
    v.editor.cancel_gesture();
    v.editor.cancel_text();
    v.editor.document.clear();
    v.editor.select_tool(Tool::Edit);
    // 与所造聚光灯层的共享颜色/线宽一致，避免测试种子造成撤销后的预设同步差异。
    v.editor.restore_tool_styles(
        [Tool::Rectangle, Tool::Spotlight, Tool::Arrow]
            .map(|t| (t, crate::tool_presets::default_style(t))),
    );
    let mut ids = Vec::new();
    for (tool, radius, a, b) in [
        (Tool::Rectangle, 8., (80., 80.), (260., 200.)),
        (Tool::Spotlight, 13., (320., 90.), (510., 220.)),
        (Tool::Arrow, 31., (560., 100.), (720., 210.)),
    ] {
        let mut style = crate::tool_presets::default_style(tool);
        style.radius = radius;
        let mut a = Annotation::new(
            tool,
            Point {
                x: area.x as f32 + a.0,
                y: area.y as f32 + a.1,
            },
            style,
        );
        a.update(Point {
            x: area.x as f32 + b.0,
            y: area.y as f32 + b.1,
        });
        ids.push(v.editor.document.insert(a));
    }
    let mut redo = v.editor.document.get(ids[0]).unwrap().clone();
    redo.style.color ^= 0x010101;
    v.editor.document.replace(ids[0], redo);
    v.editor.document.undo();
    v.editor.selected = ids.last().copied();
    v.editor.style = v.editor.document.get(*ids.last().unwrap()).unwrap().style;
    v.editor.selection = ids;
    v.toolbar_panel = ToolbarPanel::None;
    v.ui_positions.borrow_mut().clear();
}
async fn mixed(v: &WeakEntity<CaptureView>, controls: Controls, cx: &mut AsyncApp) -> Result<()> {
    frame(cx).await;
    frame(cx).await;
    let before = state(v, cx).await?;
    if !before.redo
        || before.objects.len() != 3
        || before.selection.len() != 3
        || before
            .objects
            .last()
            .is_none_or(|o| o.annotation.tool != Tool::Arrow)
    {
        return Err("混合圆角专项缺少不支持工具主选/多选/重做种子".into());
    }
    let input = open(v, controls, cx).await?;
    // 混合初始值应为空；没有输入就确认，不能把代表值8写到13px的孔。
    click(v, input, "parameter-apply", cx).await?;
    panel(v, ToolbarPanel::None, cx).await?;
    if state(v, cx).await? != before {
        return Err("混合空值确认统一了半径或破坏了重做/预设".into());
    }
    let input = open(v, controls, cx).await?;
    digits(input, "17")?;
    frame(cx).await;
    if state(v, cx).await? != before {
        return Err("多选圆角数值草稿提前修改了对象/历史/预设".into());
    }
    click(v, input, "parameter-apply", cx).await?;
    panel(v, ToolbarPanel::None, cx).await?;
    let after = state(v, cx).await?;
    let mut expected = before.objects.clone();
    for object in &mut expected {
        if object.annotation.tool.supports_radius() {
            object.annotation.style.radius = 17.;
        }
    }
    if after.objects != expected
        || after.edit_revision != before.edit_revision + 1
        || after.revision <= before.revision
        || after.redo
        || after.presets != before.presets
        || after.selected != before.selected
        || after.selection != before.selection
        || after.crop != before.crop
        || after.tool != before.tool
    {
        return Err(format!("多选17没有仅修改支持对象的一次事务：{before:?} → {after:?}").into());
    }
    v.update(cx, |v, cx| -> Result<()> {
        v.editor.undo();
        if v.editor.document.items() != before.objects
            || !v.editor.document.can_redo()
            || v.editor.tool_styles().collect::<HashMap<_, _>>() != before.presets
        {
            return Err("一次撤销没有恢复全部支持对象/重做/预设".into());
        }
        // undo 会清选择；同一批对象重新选中只作为后续取消用例的种子。
        v.editor.selected = before.selected;
        v.editor.selection = before.selection.clone();
        v.editor.style = before.style;
        cx.notify();
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    frame(cx).await;
    let undo = state(v, cx).await?;
    let input = open(v, controls, cx).await?;
    digits(input, "27")?;
    frame(cx).await;
    if state(v, cx).await? != undo {
        return Err("取消前圆角草稿已改写对象/预设".into());
    }
    input.key(0x1b);
    frame(cx).await;
    panel(v, ToolbarPanel::None, cx).await?;
    if state(v, cx).await? != undo {
        return Err("Esc取消多选圆角未保留混值/重做/预设".into());
    }
    Ok(())
}

pub(crate) async fn verify_capture(
    v: &WeakEntity<CaptureView>,
    input: Input,
    area: Rect,
    cx: &mut AsyncApp,
) -> Result<()> {
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    visible(input);
    v.update(cx, |v, cx| {
        seed(v, area);
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    mixed(
        v,
        Controls {
            entry: input,
            properties: None,
        },
        cx,
    )
    .await?;
    pass("native_radius_bulk_capture_mixed_exact_noop_cancel_one_undo_presets");
    let (id, roi, samples) = v
        .update(cx, |v, cx| {
            v.editor.cancel_gesture();
            v.editor.document.clear();
            v.editor.select_tool(Tool::Edit);
            let mut a = Annotation::new(
                Tool::Rectangle,
                Point {
                    x: area.x as f32 + 160.,
                    y: area.y as f32 + 130.,
                },
                Style {
                    filled: true,
                    radius: 40.,
                    width: 3.,
                    color: 0x2da36f,
                    opacity: 1.,
                    ..Style::default()
                },
            );
            a.update(Point {
                x: area.x as f32 + 460.,
                y: area.y as f32 + 330.,
            });
            a.rotation = 27.;
            let b = a.local_bounds();
            let samples = [(1., 1.), (-5., 100.), (150., 100.), (3., 100.)].map(|(x, y)| {
                a.to_world(Point {
                    x: b.x + x,
                    y: b.y + y,
                })
            });
            let id = v.editor.document.insert(a);
            let roi = Rect {
                x: area.x + 80,
                y: area.y + 40,
                width: 500,
                height: 400,
            };
            v.editor.crop = Some(roi);
            v.finish_style = Default::default();
            v.toolbar_panel = ToolbarPanel::None;
            cx.notify();
            (id, roi, samples)
        })
        .map_err(|e| e.to_string())?;
    let first = v
        .update(cx, |v, _| v.selected_content_image())
        .map_err(|e| e.to_string())??;
    v.update(cx, |v, cx| {
        let mut a = v.editor.document.get(id).unwrap().clone();
        a.style.width = 80.;
        v.editor.document.replace(id, a);
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    let second = v
        .update(cx, |v, _| v.selected_content_image())
        .map_err(|e| e.to_string())??;
    if roi.x == 0
        || roi.y == 0
        || (first.width, first.height) != (roi.width, roi.height)
        || (second.width, second.height) != (roi.width, roi.height)
        || first.bgra != second.bgra
    {
        return Err("填充矩形圆角导出随隐藏线宽3→80扩张，或非零ROI尺寸错误".into());
    }
    for (index, p) in samples.into_iter().enumerate() {
        let x = p.x.round() as u32;
        let y = p.y.round() as u32;
        if !roi.contains((x as i32, y as i32)) {
            return Err("填充矩形旋转样本越出自有ROI".into());
        }
        let at = ((y - roi.y) * roi.width + x - roi.x) as usize * 4;
        let expected = if index < 2 {
            [255, 255, 255, 255]
        } else {
            [0x6f, 0xa3, 0x2d, 255]
        };
        if second.bgra[at..at + 4] != expected {
            return Err(format!(
                "填充矩形圆角/外沿/内部旋转像素错误：sample={index}, actual={:?}",
                &second.bgra[at..at + 4]
            )
            .into());
        }
    }
    // 此项使用生产 selected_content_image/Scene/raster 导出，不声称检验屏幕 GPUI 像素。
    pass("native_radius_bulk_filled_rotated_nonzero_roi_scene_fill_width_invariant");
    Ok(())
}

struct Bars {
    primary: WindowHandle<PinToolbar>,
    properties: WindowHandle<PinPropertyBar>,
    primary_native: HWND,
    secondary: Input,
    primary_rect: Placement,
    secondary_rect: Placement,
}
async fn bars(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Bars> {
    let primary = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("贴图一级栏缺失")?;
    let properties = primary
        .update(cx, |v, _, _| v.properties)
        .map_err(|e| e.to_string())?
        .ok_or("贴图二级栏缺失")?;
    let primary_native = primary
        .update(cx, |_, w, _| hwnd(w))
        .map_err(|e| e.to_string())?;
    let secondary = properties
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(secondary.handle as *mut _))?;
    Ok(Bars {
        primary,
        properties,
        primary_native,
        secondary,
        primary_rect: crate::pin_native::rect(primary_native)?,
        secondary_rect: crate::pin_native::rect(HWND(secondary.handle as *mut _))?,
    })
}
async fn stable(pin: WindowHandle<PinView>, b: &Bars, cx: &mut AsyncApp) -> Result<()> {
    let current = bars(pin, cx).await?;
    if current.primary != b.primary
        || current.properties != b.properties
        || current.primary_native != b.primary_native
        || current.secondary.handle != b.secondary.handle
        || current.primary_rect != b.primary_rect
        || current.secondary_rect != b.secondary_rect
    {
        return Err("圆角数值操作重建/移动了贴图一级或二级工具栏".into());
    }
    Ok(())
}
pub(crate) async fn verify_pin(
    pin: WindowHandle<PinView>,
    input: Input,
    cx: &mut AsyncApp,
) -> Result<()> {
    visible(input);
    frame(cx).await;
    input.key(0x20);
    frame(cx).await;
    frame(cx).await;
    let canvas = pin
        .update(cx, |v, _, _| v.canvas.clone())
        .map_err(|e| e.to_string())?
        .ok_or("贴图圆角画布未创建")?;
    canvas
        .update(cx, |v, cx| {
            v.pin_editing = true;
            seed(v, Rect::full(800, 500));
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
    frame(cx).await;
    frame(cx).await;
    let b = bars(pin, cx).await?;
    let weak = canvas.downgrade();
    mixed(
        &weak,
        Controls {
            entry: b.secondary,
            properties: Some(b.properties),
        },
        cx,
    )
    .await?;
    stable(pin, &b, cx).await?;
    canvas
        .update(cx, |v, cx| {
            v.editor.document.clear();
            v.editor.select_tool(Tool::Edit);
            let mut a = Annotation::new(
                Tool::Rectangle,
                Point { x: 300., y: 250. },
                Style {
                    radius: 0.,
                    ..Style::default()
                },
            );
            a.update(Point { x: 312., y: 258. });
            let id = v.editor.document.insert(a.clone());
            v.editor.selected = Some(id);
            v.editor.style = a.style;
            v.toolbar_panel = ToolbarPanel::None;
            v.ui_positions.borrow_mut().clear();
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
    frame(cx).await;
    frame(cx).await;
    let small_bars = bars(pin, cx).await?;
    if small_bars.primary != b.primary
        || small_bars.properties != b.properties
        || small_bars.primary_rect != b.primary_rect
    {
        return Err("换为小矩形数值编辑重建两级栏或移动一级栏".into());
    }
    let small = state(&weak, cx).await?;
    let hidden = weak
        .update(cx, |v, _| {
            let zoom = v.image_canvas.as_ref().map_or(1., |s| s.viewport.zoom);
            v.editor.radius_controls(6. * input.scale / zoom).is_empty()
        })
        .map_err(|e| e.to_string())?;
    if !hidden {
        return Err("12×8小矩形意外与缩放节点争抢圆角把手".into());
    }
    let parameter = open(
        &weak,
        Controls {
            entry: small_bars.secondary,
            properties: Some(small_bars.properties),
        },
        cx,
    )
    .await?;
    digits(parameter, "17")?;
    frame(cx).await;
    if state(&weak, cx).await? != small {
        return Err("小矩形圆角草稿提前提交".into());
    }
    click(&weak, parameter, "parameter-apply", cx).await?;
    panel(&weak, ToolbarPanel::None, cx).await?;
    let after = state(&weak, cx).await?;
    let mut expected = small.objects.clone();
    expected[0].annotation.style.radius = 17.;
    if after.objects != expected || after.edit_revision != small.edit_revision + 1 || after.redo {
        return Err("没有把手的小矩形无法精确提交17px圆角的一次历史".into());
    }
    stable(pin, &small_bars, cx).await?;
    pass("native_radius_bulk_pin_two_bars_mixed_and_small_parameter");
    Ok(())
}
