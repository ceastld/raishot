//! 精确参数原生专项：真实 KEYDOWN 字符转换、越条滑动与高分辨率滚轮。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture_view::CaptureView,
    document::{Object, ObjectId},
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_view::PinView,
    selection_finish::{FinishField, FinishLooks, FinishPreset, SelectionFinishStyle},
    spotlight_smoke::frame,
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{App, AppContext, AsyncApp, Entity, WeakEntity, Window, WindowHandle};
use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
    time::Duration,
};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

fn pass(test: &str) {
    println!("{{\"test\":\"{test}\",\"ok\":true}}");
}
fn visible(input: Input) {
    unsafe {
        let h = HWND(input.handle as *mut _);
        if !IsWindowVisible(h).as_bool() {
            let _ = ShowWindow(h, SW_SHOW);
            let _ = ShowWindow(h, SW_SHOW);
        }
    }
}
fn seed(v: &mut CaptureView, area: Rect, tool: Tool, style: Style) -> ObjectId {
    v.editor.cancel_gesture();
    v.editor.cancel_text();
    v.editor.document.clear();
    let mut a = Annotation::new(
        tool,
        Point {
            x: area.x as f32 + 160.,
            y: area.y as f32 + 130.,
        },
        style,
    );
    a.update(Point {
        x: area.x as f32 + 460.,
        y: area.y as f32 + 330.,
    });
    if tool.text() {
        a.text = "参数编辑".into();
    }
    let id = v.editor.document.insert(a.clone());
    let mut other = a.clone();
    other.style.color ^= 0x010101;
    v.editor.document.replace(id, other);
    v.editor.document.undo();
    v.editor.select_tool(Tool::Edit);
    v.editor.selected = Some(id);
    v.editor.style = style;
    v.toolbar_panel = ToolbarPanel::None;
    v.ui_positions.borrow_mut().clear();
    id
}
async fn position(
    v: &WeakEntity<CaptureView>,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<(f32, f32)> {
    for _ in 0..20 {
        if let Some(p) = v
            .update(cx, |v, _| v.ui_positions.borrow().get(id).copied())
            .map_err(|e| e.to_string())?
        {
            return Ok(p);
        }
        frame(cx).await;
    }
    Err(format!("参数控件未绘制：{id}").into())
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
fn send(input: Input, message: u32, p: (f32, f32), down: bool) -> Result<()> {
    let x = (p.0 * input.scale).round() as i32;
    let y = (p.1 * input.scale).round() as i32;
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            message,
            WPARAM(if message == WM_MOUSEMOVE {
                gpui_smoke::SMOKE_MOVE | usize::from(down)
            } else {
                usize::from(down)
            }),
            LPARAM((((y as u16 as u32) << 16) | x as u16 as u32) as isize),
        )?;
    }
    Ok(())
}
fn digits(input: Input, text: &str) -> Result<()> {
    // 不直接投递 WM_CHAR；经过 GPUI 的未消费 KEYDOWN 和 Windows TranslateMessage。
    for _ in 0..16 {
        input.key(8);
    }
    for digit in text.bytes() {
        if !digit.is_ascii_digit() {
            return Err("专项仅使用不依赖键盘布局的数字键".into());
        }
        input.key(digit as usize);
    }
    Ok(())
}
fn state(v: &CaptureView, id: ObjectId) -> Result<(Style, u64, bool, Tool)> {
    Ok((
        v.editor.document.get(id).ok_or("参数目标已丢失")?.style,
        v.editor.document.revision(),
        v.editor.document.can_redo(),
        v.editor.tool,
    ))
}
async fn assert_unchanged(
    v: &WeakEntity<CaptureView>,
    id: ObjectId,
    before: (Style, u64, bool, Tool),
    cx: &mut AsyncApp,
) -> Result<()> {
    let current = v
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    if current != before {
        return Err(format!("参数草稿修改文档、历史或切换工具：{before:?} → {current:?}").into());
    }
    Ok(())
}
async fn radius_slider(
    v: &WeakEntity<CaptureView>,
    input: Input,
    id: ObjectId,
    cx: &mut AsyncApp,
) -> Result<()> {
    let before = v
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    let left = position(v, "parameter-slider-start", cx).await?;
    let right = position(v, "parameter-slider-end", cx).await?;
    send(input, WM_LBUTTONDOWN, left, true)?;
    frame(cx).await;
    // 移出 30 DIP 的条高、移出弹窗边界后仍在同一自有窗口更新到上限。
    send(input, WM_MOUSEMOVE, (right.0 + 70., right.1 + 70.), true)?;
    frame(cx).await;
    assert_unchanged(v, id, before, cx).await?;
    send(input, WM_LBUTTONUP, (right.0 + 70., right.1 + 70.), false)?;
    frame(cx).await;
    assert_unchanged(v, id, before, cx).await?;
    if v.update(cx, |v, _| v.toolbar_panel)
        .map_err(|e| e.to_string())?
        != ToolbarPanel::Radius
    {
        return Err("越界抬键误触参数提交或关闭".into());
    }
    click(v, input, "parameter-apply", cx).await?;
    let after = v
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    if after.0.radius != 50. || after.1 == before.1 || after.2 {
        return Err("越条拖动未到50或未按一次确认提交".into());
    }
    v.update(cx, |v, cx| {
        v.editor.undo();
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    let undone = v
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    if undone.0 != before.0 || !undone.2 {
        return Err("一次撤销未恢复滑条参数".into());
    }
    v.update(cx, |v, cx| {
        v.editor.selected = Some(id);
        v.editor.style = before.0;
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    frame(cx).await;
    Ok(())
}
async fn radius_wheel(
    v: &WeakEntity<CaptureView>,
    input: Input,
    id: ObjectId,
    cx: &mut AsyncApp,
) -> Result<()> {
    let before = v
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    let at = position(v, "parameter-slider", cx).await?;
    // 正向未满刻度后反向，反向新的六个20/120消息应产生-1，不能被正向余量抵消。
    input.wheel(at, 60)?;
    for _ in 0..6 {
        input.wheel(at, -20)?;
    }
    frame(cx).await;
    assert_unchanged(v, id, before, cx).await?;
    click(v, input, "parameter-apply", cx).await?;
    let after = v
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    if (after.0.radius - (before.0.radius - 1.)).abs() > 0.001 {
        return Err("高分辨率反向滚轮未按完整刻度修改草稿".into());
    }
    Ok(())
}
async fn verify_highlight_capture(
    v: &WeakEntity<CaptureView>,
    input: Input,
    area: Rect,
    cx: &mut AsyncApp,
) -> Result<()> {
    let style = crate::tool_presets::default_style(Tool::HighlighterRect);
    let id = v
        .update(cx, |v, cx| {
            let id = seed(v, area, Tool::HighlighterRect, style);
            cx.notify();
            id
        })
        .map_err(|e| e.to_string())?;
    frame(cx).await;
    frame(cx).await;
    let before = v
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    if !before.2 {
        return Err("高亮框参数专项缺少初始重做种子".into());
    }
    let before_edit_revision = v
        .update(cx, |v, _| v.editor.document.edit_revision())
        .map_err(|e| e.to_string())?;
    click(v, input, "radius-menu", cx).await?;
    digits(input, "17")?;
    frame(cx).await;
    assert_unchanged(v, id, before, cx).await?;
    click(v, input, "parameter-apply", cx).await?;
    let changed = v
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    let expected = Style {
        radius: 17.,
        ..style
    };
    let changed_edit_revision = v
        .update(cx, |v, _| v.editor.document.edit_revision())
        .map_err(|e| e.to_string())?;
    if changed.0 != expected
        || changed.1 <= before.1
        || changed_edit_revision != before_edit_revision + 1
        || changed.2
        || changed.3 != before.3
    {
        return Err(format!(
            "高亮框圆角17未按一次事务提交或改变了其他样式：{before:?} → {changed:?}"
        )
        .into());
    }
    v.update(cx, |v, cx| {
        v.editor.undo();
        v.editor.selected = Some(id);
        v.editor.style = style;
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    frame(cx).await;
    let undone = v
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    if undone.0 != style || !undone.2 {
        return Err("一次撤销未恢复高亮框圆角默认值与重做".into());
    }
    let undone_edit_revision = v
        .update(cx, |v, _| v.editor.document.edit_revision())
        .map_err(|e| e.to_string())?;
    click(v, input, "radius-menu", cx).await?;
    digits(input, "29")?;
    frame(cx).await;
    assert_unchanged(v, id, undone, cx).await?;
    input.key(0x1b);
    frame(cx).await;
    assert_unchanged(v, id, undone, cx).await?;
    if v.update(cx, |v, _| v.toolbar_panel)
        .map_err(|e| e.to_string())?
        != ToolbarPanel::None
    {
        return Err("Esc没有取消高亮框圆角草稿".into());
    }
    input.wheel(position(v, "radius-menu", cx).await?, 120)?;
    frame(cx).await;
    let wheeled = v
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    let wheeled_edit_revision = v
        .update(cx, |v, _| v.editor.document.edit_revision())
        .map_err(|e| e.to_string())?;
    if wheeled.0
        != (Style {
            radius: style.radius + 1.,
            ..style
        })
        || wheeled.1 <= undone.1
        || wheeled_edit_revision != undone_edit_revision + 1
        || wheeled.2
        || wheeled.3 != undone.3
    {
        return Err(format!("高亮框属性滚轮未只增加1圆角：{undone:?} → {wheeled:?}").into());
    }
    v.update(cx, |v, _| -> Result<()> {
        let scene = v.prepared_scene();
        if !scene.errors.is_empty() {
            return Err(scene.errors.join(";").into());
        }
        let mut output = v.source.read_region(area)?;
        scene.apply(&mut output, area);
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    pass("native_highlight_radius_capture_exact_cancel_wheel_output");
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
struct TextParameterState {
    objects: Vec<Object>,
    revision: u64,
    edit_revision: u64,
    redo: bool,
    tool: Tool,
    selected: Option<ObjectId>,
    style: Style,
    draft_id: Option<ObjectId>,
    draft: Annotation,
    selection: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
}
fn text_parameter_state(v: &CaptureView) -> Result<TextParameterState> {
    let draft = v
        .editor
        .text_draft
        .as_ref()
        .ok_or("文字参数结束了正文草稿")?;
    Ok(TextParameterState {
        objects: v.editor.document.items().to_vec(),
        revision: v.editor.document.revision(),
        edit_revision: v.editor.document.edit_revision(),
        redo: v.editor.document.can_redo(),
        tool: v.editor.tool,
        selected: v.editor.selected,
        style: v.editor.style,
        draft_id: draft.id,
        draft: draft.annotation.clone(),
        selection: v.input.selection.clone(),
        reversed: v.input.reversed,
        marked: v.input.marked.clone(),
    })
}
async fn text_state(v: &WeakEntity<CaptureView>, cx: &mut AsyncApp) -> Result<TextParameterState> {
    v.update(cx, |v, _| text_parameter_state(v))
        .map_err(|e| e.to_string())?
}
async fn panel_is(
    v: &WeakEntity<CaptureView>,
    panel: ToolbarPanel,
    cx: &mut AsyncApp,
) -> Result<()> {
    let actual = v
        .update(cx, |v, _| v.toolbar_panel)
        .map_err(|e| e.to_string())?;
    if actual != panel {
        return Err(format!("参数没有返回正确父页：{actual:?}，应为{panel:?}").into());
    }
    Ok(())
}
fn text_style_value(mut style: Style, panel: ToolbarPanel, value: f32) -> Style {
    match panel {
        ToolbarPanel::TextOutline => style.text.outline_width = value,
        ToolbarPanel::TextBackgroundAlpha => style.text.background_opacity = value / 100.,
        ToolbarPanel::TextBackgroundRadius => style.text.background_radius = value,
        _ => unreachable!(),
    }
    style
}
async fn verify_finish_text_capture(
    v: &WeakEntity<CaptureView>,
    input: Input,
    window: gpui::AnyWindowHandle,
    area: Rect,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (id, original) = v
        .update(cx, |v, cx| {
            let mut style = crate::tool_presets::default_style(Tool::Text);
            style.text.outline = true;
            style.text.background = true;
            let id = seed(v, area, Tool::Text, style);
            let mut original = v.editor.document.get(id).unwrap().clone();
            original.text = "已有中文🙂正文".into();
            v.editor.document.replace(id, original.clone());
            let mut redo = original.clone();
            redo.style.text.outline_color = 0x5796ff;
            v.editor.document.replace(id, redo);
            v.editor.document.undo();
            v.editor.edit_text();
            v.editor.set_text(original.text.clone());
            // 两端均为中文/emoji的真实UTF-8边界，反向选择检查参数输入没有接管正文选区。
            v.input.selection = "已有".len().."已有中文🙂".len();
            v.input.reversed = true;
            v.input.marked = None;
            cx.notify();
            (id, original)
        })
        .map_err(|e| e.to_string())?;
    frame(cx).await;
    frame(cx).await;
    // 描边粗细在描边细项里，底色不透明度与圆角在底色细项里；确认后回到所在细项。
    for (chip, panel, value, number) in [
        ("text-outline-width", ToolbarPanel::TextOutline, 6., "6"),
        (
            "text-background-alpha",
            ToolbarPanel::TextBackgroundAlpha,
            65.,
            "65",
        ),
        (
            "text-background-radius",
            ToolbarPanel::TextBackgroundRadius,
            17.,
            "17",
        ),
    ] {
        let before = text_state(v, cx).await?;
        let (detail, detail_menu) = if panel == ToolbarPanel::TextOutline {
            (ToolbarPanel::TextOutlineStyle, "text-outline-menu")
        } else {
            (ToolbarPanel::TextBackgroundStyle, "text-background-menu")
        };
        if v.update(cx, |v, _| v.toolbar_panel)
            .map_err(|e| e.to_string())?
            != detail
        {
            click(v, input, detail_menu, cx).await?;
            panel_is(v, detail, cx).await?;
        }
        click(v, input, chip, cx).await?;
        panel_is(v, panel, cx).await?;
        digits(input, number)?;
        frame(cx).await;
        if text_state(v, cx).await? != before {
            return Err(format!("文字{panel:?}数值草稿修改了正文/光标/历史").into());
        }
        click(v, input, "parameter-apply", cx).await?;
        panel_is(v, detail, cx).await?;
        let mut expected = before;
        expected.style = text_style_value(expected.style, panel, value);
        expected.draft.style = text_style_value(expected.draft.style, panel, value);
        if text_state(v, cx).await? != expected {
            return Err(format!("文字{panel:?}确认未只改草稿单字段或丢失正文/光标").into());
        }
    }
    let before_cancel = text_state(v, cx).await?;
    click(v, input, "text-background-radius", cx).await?;
    digits(input, "23")?;
    frame(cx).await;
    if text_state(v, cx).await? != before_cancel {
        return Err("文字圆角取消前草稿已泄漏".into());
    }
    input.key(0x1b);
    frame(cx).await;
    panel_is(v, ToolbarPanel::TextBackgroundStyle, cx).await?;
    if text_state(v, cx).await? != before_cancel {
        return Err("Esc取消文字圆角丢失草稿或光标".into());
    }
    input.key(0x1b);
    frame(cx).await;
    panel_is(v, ToolbarPanel::None, cx).await?;
    if text_state(v, cx).await? != before_cancel {
        return Err("关闭文字父页结束了正文或修改了光标".into());
    }
    v.update(cx, |v, cx| -> Result<()> {
        v.editor.finish_text();
        if v.editor.document.get(id) != Some(&before_cancel.draft)
            || v.editor.document.edit_revision() != before_cancel.edit_revision + 1
            || v.editor.document.can_redo()
        {
            return Err("显式完成文字未按一次事务提交整个草稿".into());
        }
        v.editor.undo();
        if v.editor.document.get(id) != Some(&original) || !v.editor.document.can_redo() {
            return Err("一次撤销未恢复原有中文🙂文字对象".into());
        }
        cx.notify();
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    pass("native_finish_text_exact_parent_draft_caret_cancel_undo");
    verify_finish_parameters_capture(v, input, window, area, cx).await
}

#[derive(Clone, Debug, PartialEq)]
struct FinishParameterState {
    style: SelectionFinishStyle,
    preset: FinishPreset,
    objects: Vec<Object>,
    revision: u64,
    edit_revision: u64,
    redo: bool,
    tool: Tool,
    crop: Option<Rect>,
}
fn finish_parameter_state(v: &CaptureView) -> FinishParameterState {
    FinishParameterState {
        style: v.finish_style,
        preset: v.finish_preset,
        objects: v.editor.document.items().to_vec(),
        revision: v.editor.document.revision(),
        edit_revision: v.editor.document.edit_revision(),
        redo: v.editor.document.can_redo(),
        tool: v.editor.tool,
        crop: v.editor.crop,
    }
}
async fn finish_state(
    v: &WeakEntity<CaptureView>,
    cx: &mut AsyncApp,
) -> Result<FinishParameterState> {
    v.update(cx, |v, _| finish_parameter_state(v))
        .map_err(|e| e.to_string().into())
}
/// 点中内联数值框并清空旧值；空草稿无效，不改样式。
async fn clear_finish_value(
    v: &WeakEntity<CaptureView>,
    input: Input,
    window: gpui::AnyWindowHandle,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    click(v, input, id, cx).await?;
    // 一次删除整值，避免 12→2→空 的有效中间值触发实时应用。
    let select_all = gpui::Keystroke::parse("ctrl-a").map_err(|e| e.to_string())?;
    cx.update_window(window, |_, w, cx| w.dispatch_keystroke(select_all, cx))
        .map_err(|e| e.to_string())?;
    input.key(0x2e);
    frame(cx).await;
    Ok(())
}
async fn verify_finish_parameters_capture(
    v: &WeakEntity<CaptureView>,
    input: Input,
    window: gpui::AnyWindowHandle,
    area: Rect,
    cx: &mut AsyncApp,
) -> Result<()> {
    v.update(cx, |v, cx| {
        v.editor.cancel_text();
        v.editor.select_tool(Tool::Crop);
        // 取白底远离文字的自有小区，避免整张虚拟桌面反复合成。
        v.editor.crop = Some(Rect {
            x: area.x + 680,
            y: area.y + 440,
            width: 300,
            height: 180,
        });
        // 从原样与各样式出厂外观开始，不受此前会话记住的样式影响。
        v.finish_style = SelectionFinishStyle::default();
        v.finish_preset = FinishPreset::Raw;
        v.finish_looks = FinishLooks::default();
        v.finish_hover = None;
        v.finish_preview = None;
        v.toolbar_panel = ToolbarPanel::None;
        v.ui_positions.borrow_mut().clear();
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    frame(cx).await;
    frame(cx).await;
    // 原样没有参数：先在选区条换上“圆角”，“调整”只改这个样式。
    // 条收起成单按钮时，画廊面板里同时带样式与当前样式的调整项。
    let inline = v
        .update(cx, |v, _| {
            v.ui_positions.borrow().contains_key("selection-look-round")
        })
        .map_err(|e| e.to_string())?;
    if inline {
        click(v, input, "selection-look-round", cx).await?;
        click(v, input, "selection-style-tune", cx).await?;
    } else {
        click(v, input, "selection-style-menu", cx).await?;
        click(v, input, "finish-look-round", cx).await?;
    }
    panel_is(v, ToolbarPanel::Finish, cx).await?;
    if finish_state(v, cx).await?.style != FinishPreset::Round.defaults() {
        return Err("选区条“圆角”未换上它的出厂外观".into());
    }
    for (value_id, field, number) in [
        ("finish-radius-value", FinishField::Radius, "17"),
        ("finish-blur-value", FinishField::Blur, "24"),
        ("finish-opacity-value", FinishField::Opacity, "50"),
        ("finish-thickness-value", FinishField::Thickness, "4"),
    ] {
        panel_is(v, ToolbarPanel::Finish, cx).await?;
        let before = finish_state(v, cx).await?;
        // 有效值实时生效；清空后的空草稿无效，Esc 只撤回草稿，不关闭面板。
        clear_finish_value(v, input, window, value_id, cx).await?;
        if finish_state(v, cx).await? != before {
            return Err("选区参数的无效草稿改动了样式/历史".into());
        }
        input.key(0x1b);
        frame(cx).await;
        panel_is(v, ToolbarPanel::Finish, cx).await?;
        if finish_state(v, cx).await? != before {
            return Err("撤回选区参数草稿修改了样式/文档".into());
        }
        clear_finish_value(v, input, window, value_id, cx).await?;
        digits(input, number)?;
        frame(cx).await;
        input.key(0x0d);
        frame(cx).await;
        panel_is(v, ToolbarPanel::Finish, cx).await?;
        let mut expected = before;
        match field {
            FinishField::Radius => {
                expected.style.corner_radius = 17.;
                expected.style.corner_enabled = true;
            }
            FinishField::Blur => {
                expected.style.shadow_blur = 24.;
                expected.style.shadow_enabled = true;
            }
            FinishField::Opacity => {
                expected.style.shadow_opacity = 0.5;
                expected.style.shadow_enabled = true;
            }
            FinishField::Thickness => {
                expected.style.border_thickness = 4.;
                expected.style.border_enabled = true;
            }
        }
        if finish_state(v, cx).await? != expected {
            return Err(format!("选区{field:?}设值修改了无关字段/文档或数值不准确").into());
        }
    }
    // 没有效果开关：圆角设为 0 即关闭并保留 17px 档位，从 0 设值即重新开启。
    let enabled = finish_state(v, cx).await?;
    clear_finish_value(v, input, window, "finish-radius-value", cx).await?;
    digits(input, "0")?;
    frame(cx).await;
    input.key(0x0d);
    frame(cx).await;
    panel_is(v, ToolbarPanel::Finish, cx).await?;
    let mut disabled = enabled.clone();
    disabled.style.corner_enabled = false;
    if finish_state(v, cx).await? != disabled {
        return Err("圆角0没有关闭效果并保留17px档位".into());
    }
    clear_finish_value(v, input, window, "finish-radius-value", cx).await?;
    digits(input, "17")?;
    frame(cx).await;
    input.key(0x0d);
    frame(cx).await;
    if finish_state(v, cx).await? != enabled {
        return Err("圆角从0设回17没有重新开启".into());
    }
    v.update(cx, |v, _| -> Result<()> {
        let before = finish_parameter_state(v);
        let crop = v.editor.crop.ok_or("选区参数丢失选区")?;
        let origin = v.finished_origin(crop);
        let output = v.selected_image()?;
        // 阴影从外侧边框外沿投出：边框 4 + 扩散 24。
        let pad = 28;
        if (output.width, output.height) != (crop.width + 2 * pad, crop.height + 2 * pad)
            || origin
                != (
                    v.source.bounds.left + crop.x as i32 - pad as i32,
                    v.source.bounds.top + crop.y as i32 - pad as i32,
                )
            || v.effective_finish_style().padding().left != pad
            || v.finish_preset != FinishPreset::Round
        {
            return Err("精调选区导出padding、尺寸、全局原点或当前样式错误".into());
        }
        let center = ((pad + crop.height / 2) * output.width + pad + crop.width / 2) as usize * 4;
        if output.bgra[3] != 0
            || output.bgra[center..center + 4] != [255; 4]
            || !output
                .bgra
                .chunks_exact(4)
                .any(|pixel| pixel[3] > 0 && pixel[3] < 255)
        {
            return Err("精调选区输出缺少透明边角、半透明阴影或内容原像素".into());
        }
        if finish_parameter_state(v) != before {
            return Err("输出精调选区修改了文档/样式".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    pass("native_finish_parameters_exact_cancel_transparent_export");
    Ok(())
}

async fn verify_capture(
    v: WeakEntity<CaptureView>,
    input: Input,
    window: gpui::AnyWindowHandle,
    cx: &mut AsyncApp,
) -> Result<()> {
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    visible(input);
    let area = v
        .update(cx, |v, cx| {
            let area = crate::spotlight_smoke::install(v);
            cx.notify();
            area
        })
        .map_err(|e| e.to_string())??;
    if std::env::var("RUSTSHOT_PARAMETER_CAPTURE_CASE").as_deref() == Ok("highlight") {
        return verify_highlight_capture(&v, input, area, cx).await;
    }
    if std::env::var("RUSTSHOT_PARAMETER_CAPTURE_CASE").as_deref() == Ok("finish_text") {
        return verify_finish_text_capture(&v, input, window, area, cx).await;
    }
    if std::env::var("RUSTSHOT_PARAMETER_CAPTURE_CASE").as_deref() == Ok("selection_effect") {
        return crate::selection_effect_smoke::verify_capture(&v, input, window, area, cx).await;
    }
    if std::env::var("RUSTSHOT_PARAMETER_CAPTURE_CASE").as_deref() == Ok("radius_bulk") {
        return crate::radius_bulk_smoke::verify_capture(&v, input, area, cx).await;
    }
    let id = v
        .update(cx, |v, cx| {
            let id = seed(
                v,
                area,
                Tool::Rectangle,
                Style {
                    width: 4.,
                    radius: 17.,
                    ..Style::default()
                },
            );
            cx.notify();
            id
        })
        .map_err(|e| e.to_string())?;
    frame(cx).await;
    frame(cx).await;
    let case = std::env::var("RUSTSHOT_PARAMETER_QA_CASE").unwrap_or_else(|_| "all".into());
    let runs = |name| case == "all" || case == name;
    if runs("width") {
        let before = v
            .update(cx, |v, _| state(v, id))
            .map_err(|e| e.to_string())??;
        click(&v, input, "width-menu", cx).await?;
        digits(input, "13")?;
        frame(cx).await;
        assert_unchanged(&v, id, before, cx).await?;
        click(&v, input, "parameter-apply", cx).await?;
        let changed = v
            .update(cx, |v, _| state(v, id))
            .map_err(|e| e.to_string())??;
        if changed.0.width != 13. || changed.0.radius != 17. {
            return Err("原生数字键未输入普通线宽13，或误触工具快捷键".into());
        }
        v.update(cx, |v, cx| {
            v.editor.undo();
            v.editor.selected = Some(id);
            v.editor.style = before.0;
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
        frame(cx).await;
        click(&v, input, "width-menu", cx).await?;
        digits(input, "22")?;
        frame(cx).await;
        input.key(0x1b);
        frame(cx).await;
        if v.update(cx, |v, _| state(v, id))
            .map_err(|e| e.to_string())??
            .0
            != before.0
            || !v
                .update(cx, |v, _| v.editor.document.can_redo())
                .map_err(|e| e.to_string())?
        {
            return Err("Esc取消宽度草稿破坏了对象或重做".into());
        }
        pass("native_parameter_width_real_keydown_draft_confirm_once_escape_preserves_redo");
    }
    if runs("slider") {
        click(&v, input, "radius-menu", cx).await?;
        radius_slider(&v, input, id, cx).await?;
        pass("native_parameter_slider_outside_track_popup_continuity_atomic_undo");
    }
    if runs("wheel") {
        click(&v, input, "radius-menu", cx).await?;
        radius_wheel(&v, input, id, cx).await?;
        pass("native_parameter_high_resolution_wheel_direction_draft_isolation");
    }
    if runs("selection") {
        let (first, second) = v
            .update(cx, |v, cx| {
                let first = seed(
                    v,
                    area,
                    Tool::Rectangle,
                    Style {
                        width: 4.,
                        ..Style::default()
                    },
                );
                let mut a = v.editor.document.get(first).unwrap().clone();
                a.translate(340., 0.);
                a.style.width = 20.;
                let second = v.editor.document.insert(a);
                cx.notify();
                (first, second)
            })
            .map_err(|e| e.to_string())?;
        frame(cx).await;
        let at = (
            (area.x as f32 + 70.) / input.scale,
            (area.y as f32 + 70.) / input.scale,
        );
        input.wheel(at, 72)?;
        frame(cx).await;
        let key = gpui::Keystroke::parse("ctrl-a").map_err(|e| e.to_string())?;
        cx.update_window(window, |_, w, cx| w.dispatch_keystroke(key, cx))
            .map_err(|e| e.to_string())?;
        frame(cx).await;
        input.wheel(at, 48)?;
        frame(cx).await;
        let check = v
            .update(cx, |v, _| {
                (
                    v.editor.style.width,
                    v.editor.document.get(first).unwrap().style.width,
                    v.editor.document.get(second).unwrap().style.width,
                )
            })
            .map_err(|e| e.to_string())?;
        if check != (20., 4., 20.) {
            return Err("全选沿用了旧对象滚轮余量或参数基准".into());
        }
        input.wheel(at, 72)?;
        frame(cx).await;
        let check = v
            .update(cx, |v, _| {
                (
                    v.editor.document.get(first).unwrap().style.width,
                    v.editor.document.get(second).unwrap().style.width,
                )
            })
            .map_err(|e| e.to_string())?;
        if check != (21., 21.) {
            return Err("全选后新滚轮刻度未按主选20正确修改到21".into());
        }
        pass("gpui_parameter_select_all_resets_native_wheel_remainder_and_primary_style");
    }
    if runs("font_effect") {
        let text = v
            .update(cx, |v, cx| {
                let id = seed(
                    v,
                    area,
                    Tool::Text,
                    Style {
                        font_size: 28.,
                        ..Style::default()
                    },
                );
                cx.notify();
                id
            })
            .map_err(|e| e.to_string())?;
        frame(cx).await;
        click(&v, input, "font-menu", cx).await?;
        digits(input, "37")?;
        frame(cx).await;
        click(&v, input, "parameter-apply", cx).await?;
        if v.update(cx, |v, _| state(v, text))
            .map_err(|e| e.to_string())??
            .0
            .font_size
            != 37.
        {
            return Err("字号精确输入未提交37".into());
        }
        let mosaic = v
            .update(cx, |v, cx| {
                let id = seed(
                    v,
                    area,
                    Tool::Mosaic,
                    Style {
                        block_size: 16,
                        ..Style::default()
                    },
                );
                cx.notify();
                id
            })
            .map_err(|e| e.to_string())?;
        frame(cx).await;
        click(&v, input, "block-menu", cx).await?;
        digits(input, "2")?;
        frame(cx).await;
        click(&v, input, "parameter-apply", cx).await?;
        if v.update(cx, |v, _| state(v, mosaic))
            .map_err(|e| e.to_string())??
            .0
            .block_size
            != 2
        {
            return Err("效果强度精确输入未提交2".into());
        }
        v.update(cx, |v, _| -> Result<()> {
            let scene = v.prepared_scene();
            if !scene.errors.is_empty() {
                return Err(scene.errors.join(";").into());
            }
            let mut output = v.source.read_region(area)?;
            scene.apply(&mut output, area);
            Ok(())
        })
        .map_err(|e| e.to_string())??;
        pass("native_parameter_font_and_effect_integer_two_shared_scene_output");
    }
    Ok(())
}
pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    *errors.borrow_mut() = Some("精确参数专项未完成".into());
    let weak = view.downgrade();
    let input = Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    };
    let handle = window.window_handle();
    window.on_next_frame(move |_, cx| {
        let done = Rc::new(Cell::new(false));
        let timeout_done = done.clone();
        let timeout_errors = errors.clone();
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_secs(45))
                .await;
            if !timeout_done.get() {
                *timeout_errors.borrow_mut() = Some("精确参数专项超时".into());
                let _ = cx.update(|cx| cx.quit());
            }
        })
        .detach();
        cx.spawn(async move |cx| {
            let result = verify_capture(weak, input, handle, cx).await;
            done.set(true);
            *errors.borrow_mut() = result.err().map(|e| e.to_string());
            let _ = cx.update(|cx| cx.quit());
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(input.handle as *mut _)),
                    WM_NULL,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        })
        .detach();
    });
}
async fn text_popup_input(
    properties: WindowHandle<crate::pin_property_bar::PinPropertyBar>,
    panel: ToolbarPanel,
    cx: &mut AsyncApp,
) -> Result<Input> {
    for _ in 0..20 {
        let popup = properties
            .update(cx, |v, _, _| {
                (v.popup_panel == panel).then_some(v.popup).flatten()
            })
            .map_err(|e| e.to_string())?;
        let input = popup.and_then(|popup| {
            popup
                .update(cx, |_, w, _| Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                })
                .ok()
        });
        if let Some(input) = input {
            gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
            visible(input);
            // 子页会换 HWND；等待这个窗口自己的输入控件绘制和初始全文选中。
            frame(cx).await;
            return Ok(input);
        }
        frame(cx).await;
    }
    Err(format!("贴图文字参数页未创建：{panel:?}").into())
}
async fn verify_finish_text_pin(
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
        .ok_or("贴图文字画布未创建")?;
    canvas
        .update(cx, |v, cx| {
            v.pin_editing = true;
            v.editor.cancel_gesture();
            v.editor.cancel_text();
            v.editor.document.clear();
            v.editor.select_tool(Tool::Text);
            let mut style = crate::tool_presets::default_style(Tool::Text);
            style.text.outline = true;
            style.text.background = true;
            v.editor.set_style(style);
            v.editor.begin_text(None, Point { x: 70., y: 70. });
            v.editor.set_text("贴图中文🙂".into());
            let len = "贴图中文🙂".len();
            v.input.selection = len..len;
            v.input.reversed = false;
            v.input.marked = None;
            v.toolbar_panel = ToolbarPanel::None;
            v.ui_positions.borrow_mut().clear();
            cx.notify();
        })
        .map_err(|e| e.to_string())?;
    // 仅在初始新建草稿时建立编辑焦点；关闭父页后的焦点必须由生产路由恢复。
    pin.update(cx, |_, w, cx| {
        w.focus(&canvas.read(cx).focus);
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    frame(cx).await;
    frame(cx).await;
    let primary = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("贴图文字一级栏未创建")?;
    let properties = primary
        .update(cx, |v, _, _| v.properties)
        .map_err(|e| e.to_string())?
        .ok_or("贴图文字二级栏未创建")?;
    let secondary = properties
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(secondary.handle as *mut _))?;
    let primary_hwnd = primary
        .update(cx, |_, w, _| hwnd(w))
        .map_err(|e| e.to_string())?;
    let secondary_hwnd = HWND(secondary.handle as *mut _);
    let primary_rect = crate::pin_native::rect(primary_hwnd)?;
    let secondary_rect = crate::pin_native::rect(secondary_hwnd)?;
    let weak = canvas.downgrade();
    let initial = text_state(&weak, cx).await?;
    if initial.draft_id.is_some() || initial.tool != Tool::Text {
        return Err("贴图文字参数专项不是新建正文草稿".into());
    }
    click(&weak, secondary, "text-background-menu", cx).await?;
    panel_is(&weak, ToolbarPanel::TextBackgroundStyle, cx).await?;
    let parent_input = text_popup_input(properties, ToolbarPanel::TextBackgroundStyle, cx).await?;
    click(&weak, parent_input, "text-background-radius", cx).await?;
    panel_is(&weak, ToolbarPanel::TextBackgroundRadius, cx).await?;
    let child_input = text_popup_input(properties, ToolbarPanel::TextBackgroundRadius, cx).await?;
    digits(child_input, "17")?;
    frame(cx).await;
    if text_state(&weak, cx).await? != initial {
        return Err("贴图文字圆角数值草稿改变了正文/光标/文档".into());
    }
    click(&weak, child_input, "parameter-apply", cx).await?;
    panel_is(&weak, ToolbarPanel::TextBackgroundStyle, cx).await?;
    let mut expected = initial;
    expected.style.text.background_radius = 17.;
    expected.draft.style.text.background_radius = 17.;
    if text_state(&weak, cx).await? != expected {
        return Err("贴图圆角确认没有仅更新正文草稿17px或丢失光标".into());
    }
    let parent_input = text_popup_input(properties, ToolbarPanel::TextBackgroundStyle, cx).await?;
    click(&weak, parent_input, "text-background-radius", cx).await?;
    let child_input = text_popup_input(properties, ToolbarPanel::TextBackgroundRadius, cx).await?;
    digits(child_input, "23")?;
    frame(cx).await;
    if text_state(&weak, cx).await? != expected {
        return Err("贴图文字取消前参数草稿已泄漏".into());
    }
    child_input.key(0x1b);
    frame(cx).await;
    panel_is(&weak, ToolbarPanel::TextBackgroundStyle, cx).await?;
    if text_state(&weak, cx).await? != expected {
        return Err("贴图Esc取消数值没有保留正文/光标/样式".into());
    }
    let parent_input = text_popup_input(properties, ToolbarPanel::TextBackgroundStyle, cx).await?;
    parent_input.key(0x1b);
    frame(cx).await;
    panel_is(&weak, ToolbarPanel::None, cx).await?;
    if text_state(&weak, cx).await? != expected {
        return Err("贴图关闭文字父页提交了正文或丢失光标".into());
    }
    let current_primary = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?;
    let current_secondary = primary
        .update(cx, |v, _, _| v.properties)
        .map_err(|e| e.to_string())?;
    if current_primary != Some(primary)
        || current_secondary != Some(properties)
        || primary
            .update(cx, |_, w, _| hwnd(w))
            .map_err(|e| e.to_string())?
            != primary_hwnd
        || properties
            .update(cx, |_, w, _| hwnd(w))
            .map_err(|e| e.to_string())?
            != secondary_hwnd
        || crate::pin_native::rect(primary_hwnd)? != primary_rect
        || crate::pin_native::rect(secondary_hwnd)? != secondary_rect
    {
        return Err("文字子页操作重建或移动了贴图两级工具栏".into());
    }
    // 真正向贴图宿主发送 KEYDOWN，由 TranslateMessage 进入恢复后的正文 input_handler。
    // 不重设焦点、不用 WM_CHAR 绕过 Pin 的快捷键消费路径。
    input.key(b'7' as usize);
    frame(cx).await;
    expected.draft.text.push('7');
    let len = expected.draft.text.len();
    expected.selection = len..len;
    expected.reversed = false;
    if text_state(&weak, cx).await? != expected {
        return Err("贴图关闭文字父页后真实数字键没有追加到原正文草稿".into());
    }
    pass("native_finish_text_pin_parent_two_bars_host_input");
    Ok(())
}
pub(crate) async fn verify_pin(
    pin: WindowHandle<PinView>,
    input: Input,
    cx: &mut AsyncApp,
) -> Result<()> {
    if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("radius_bulk_parameter") {
        return crate::radius_bulk_smoke::verify_pin(pin, input, cx).await;
    }
    if std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("finish_text_parameter") {
        return verify_finish_text_pin(pin, input, cx).await;
    }
    let highlight =
        std::env::var("RUSTSHOT_PIN_SMOKE_CASE").as_deref() == Ok("highlight_parameter");
    visible(input);
    frame(cx).await;
    input.key(0x20);
    frame(cx).await;
    frame(cx).await;
    let canvas = pin
        .update(cx, |v, _, _| v.canvas.clone())
        .map_err(|e| e.to_string())?
        .ok_or("贴图画布未创建")?;
    let id = canvas
        .update(cx, |v, cx| {
            v.pin_editing = true;
            let id = seed(
                v,
                Rect::full(800, 500),
                if highlight {
                    Tool::HighlighterRect
                } else {
                    Tool::Rectangle
                },
                if highlight {
                    crate::tool_presets::default_style(Tool::HighlighterRect)
                } else {
                    Style {
                        width: 4.,
                        radius: 17.,
                        ..Style::default()
                    }
                },
            );
            cx.notify();
            id
        })
        .map_err(|e| e.to_string())?;
    frame(cx).await;
    frame(cx).await;
    let primary = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("贴图一级栏未创建")?;
    let properties = primary
        .update(cx, |v, _, _| v.properties)
        .map_err(|e| e.to_string())?
        .ok_or("贴图二级栏未创建")?;
    let secondary = properties
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    let primary_hwnd = primary
        .update(cx, |_, w, _| hwnd(w))
        .map_err(|e| e.to_string())?;
    let before_primary_rect = crate::pin_native::rect(primary_hwnd)?;
    let before_secondary_rect = crate::pin_native::rect(HWND(secondary.handle as *mut _))?;
    gpui_smoke::isolate_input(HWND(secondary.handle as *mut _))?;
    let weak = canvas.downgrade();
    click(
        &weak,
        secondary,
        if highlight {
            "radius-menu"
        } else {
            "width-menu"
        },
        cx,
    )
    .await?;
    let popup = properties
        .update(cx, |v, _, _| v.popup)
        .map_err(|e| e.to_string())?
        .ok_or(if highlight {
            "贴图高亮框圆角参数窗未创建"
        } else {
            "贴图普通线宽参数窗未创建"
        })?;
    let popup_input = popup
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(popup_input.handle as *mut _))?;
    visible(popup_input);
    frame(cx).await;
    let before = weak
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    let before_edit_revision = if highlight {
        weak.update(cx, |v, _| v.editor.document.edit_revision())
            .map_err(|e| e.to_string())?
    } else {
        0
    };
    digits(popup_input, if highlight { "17" } else { "19" })?;
    frame(cx).await;
    assert_unchanged(&weak, id, before, cx).await?;
    click(&weak, popup_input, "parameter-apply", cx).await?;
    let changed = weak
        .update(cx, |v, _| state(v, id))
        .map_err(|e| e.to_string())??;
    if highlight {
        let changed_edit_revision = weak
            .update(cx, |v, _| v.editor.document.edit_revision())
            .map_err(|e| e.to_string())?;
        if changed.0
            != (Style {
                radius: 17.,
                ..before.0
            })
            || changed.1 <= before.1
            || changed_edit_revision != before_edit_revision + 1
            || changed.2
            || changed.3 != before.3
        {
            return Err("贴图高亮框原生数字输入未按一次事务仅提交圆角17".into());
        }
    } else if changed.0.width != 19. {
        return Err("贴图原生数字输入未提交19或触发宿主旋转".into());
    }
    let current = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?;
    let current_secondary = primary
        .update(cx, |v, _, _| v.properties)
        .map_err(|e| e.to_string())?;
    if current != Some(primary) || current_secondary != Some(properties) {
        return Err("精确参数操作重建了贴图一级或二级栏".into());
    }
    let current_primary_hwnd = primary
        .update(cx, |_, w, _| hwnd(w))
        .map_err(|e| e.to_string())?;
    let current_secondary_hwnd = properties
        .update(cx, |_, w, _| hwnd(w))
        .map_err(|e| e.to_string())?;
    if current_primary_hwnd != primary_hwnd
        || current_secondary_hwnd != HWND(secondary.handle as *mut _)
    {
        return Err("贴图两级栏的原生句柄变化".into());
    }
    if crate::pin_native::rect(primary_hwnd)? != before_primary_rect
        || crate::pin_native::rect(HWND(secondary.handle as *mut _))? != before_secondary_rect
    {
        return Err("精确参数操作移动或改变了两级贴图工具栏尺寸".into());
    }
    pass(if highlight {
        "native_highlight_radius_pin_keydown_two_bars_stable"
    } else {
        "native_parameter_pin_real_keydown_width_two_bars_stable"
    });
    Ok(())
}
