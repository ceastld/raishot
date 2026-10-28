//! 选区外观样式直选与调整的独立原生验收；输入仅投递到本次自有截图 HWND。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture_view::CaptureView,
    document::Object,
    geometry::Rect,
    gpui_smoke::{self, Input},
    selection_finish::{FinishEffect, FinishLooks, FinishPreset, SelectionFinishStyle},
    selection_finish_view::{look_id, FinishSection},
    spotlight_smoke::frame,
    toolbar_layout::ToolbarPanel,
    Result,
};
use gpui::{AppContext, AsyncApp, WeakEntity};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

#[derive(Clone, Debug, PartialEq)]
struct DocumentState {
    objects: Vec<Object>,
    revision: u64,
    edit_revision: u64,
    redo: bool,
    crop: Option<Rect>,
    tool: Tool,
}
#[derive(Clone, Debug, PartialEq)]
struct State {
    style: SelectionFinishStyle,
    preset: FinishPreset,
    effect: FinishEffect,
    intent: Option<FinishEffect>,
    advanced: bool,
    document: DocumentState,
}
fn read(v: &CaptureView) -> State {
    State {
        style: v.finish_style,
        preset: v.finish_preset,
        effect: v.finish_effect,
        intent: v.finish_effect_intent,
        advanced: v.finish_advanced,
        document: DocumentState {
            objects: v.editor.document.items().to_vec(),
            revision: v.editor.document.revision(),
            edit_revision: v.editor.document.edit_revision(),
            redo: v.editor.document.can_redo(),
            crop: v.editor.crop,
            tool: v.editor.tool,
        },
    }
}
async fn state(v: &WeakEntity<CaptureView>, cx: &mut AsyncApp) -> Result<State> {
    v.update(cx, |v, _| read(v))
        .map_err(|e| e.to_string().into())
}
async fn check(
    v: &WeakEntity<CaptureView>,
    expected: SelectionFinishStyle,
    doc: &DocumentState,
    cx: &mut AsyncApp,
) -> Result<()> {
    let actual = state(v, cx).await?;
    if actual.style != expected || &actual.document != doc {
        return Err(format!(
            "选区效果样式、原选区或文档历史错误：期望{expected:?}，实际{actual:?}"
        )
        .into());
    }
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
        return Err(format!("选区效果没有返回父页：{actual:?}，应为{expected:?}").into());
    }
    Ok(())
}
async fn at(
    v: &WeakEntity<CaptureView>,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<(f32, f32)> {
    for _ in 0..12 {
        let p = v
            .update(cx, |v, _| v.ui_positions.borrow().get(id).copied())
            .map_err(|e| e.to_string())?;
        if let Some(p) = p {
            return Ok(p);
        }
        frame(cx).await;
    }
    Err(format!("选区效果控件未绘制：{id}").into())
}
async fn click(
    v: &WeakEntity<CaptureView>,
    input: Input,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    input.click(at(v, id, cx).await?);
    frame(cx).await;
    Ok(())
}
/// 内联数值框：整值清空后逐字输入，有效值实时生效；Enter 不再重复修改样式。
async fn numeric(
    v: &WeakEntity<CaptureView>,
    input: Input,
    window: gpui::AnyWindowHandle,
    value_id: &'static str,
    number: &str,
    cx: &mut AsyncApp,
) -> Result<()> {
    click(v, input, value_id, cx).await?;
    // 只在此窗口全选后一次清空，保留设为 0 时应记住的最后档位。
    let select_all = gpui::Keystroke::parse("ctrl-a").map_err(|e| e.to_string())?;
    cx.update_window(window, |_, w, cx| w.dispatch_keystroke(select_all, cx))
        .map_err(|e| e.to_string())?;
    input.key(0x2e);
    unsafe {
        for unit in number.encode_utf16() {
            PostMessageW(
                Some(HWND(input.handle as *mut _)),
                WM_CHAR,
                WPARAM(unit as usize),
                LPARAM(0),
            )?;
        }
    }
    frame(cx).await;
    let before = state(v, cx).await?;
    input.key(0x0d);
    frame(cx).await;
    if state(v, cx).await? != before {
        return Err("内联数值 Enter 重复修改了已实时生效的样式".into());
    }
    panel(v, ToolbarPanel::Finish, cx).await
}
/// 样式按钮的位置：选区条缩略图；条收起成单按钮时打开画廊取大缩略图。
async fn look_point(
    v: &WeakEntity<CaptureView>,
    input: Input,
    look: FinishPreset,
    cx: &mut AsyncApp,
) -> Result<(f32, f32)> {
    let bar = look_id(look, false);
    let (inline, gallery) = v
        .update(cx, |v, _| {
            (
                v.ui_positions.borrow().get(bar).copied(),
                v.toolbar_panel == ToolbarPanel::Finish
                    && v.finish_section == FinishSection::Gallery,
            )
        })
        .map_err(|e| e.to_string())?;
    if let Some(p) = inline {
        return Ok(p);
    }
    // 画廊已打开时再点单按钮会把它收起。
    if !gallery {
        click(v, input, "selection-style-menu", cx).await?;
    }
    at(v, look_id(look, true), cx).await
}
/// 单击样式：换上它记住的外观（本专项从出厂外观开始），文档与选区不变。
async fn look(
    v: &WeakEntity<CaptureView>,
    input: Input,
    look: FinishPreset,
    doc: &DocumentState,
    cx: &mut AsyncApp,
) -> Result<SelectionFinishStyle> {
    input.click(look_point(v, input, look, cx).await?);
    frame(cx).await;
    let preset = v
        .update(cx, |v, _| v.finish_preset)
        .map_err(|e| e.to_string())?;
    if preset != look {
        return Err(format!("单击样式未换成{look:?}，当前为{preset:?}").into());
    }
    let expected = look.defaults();
    check(v, expected, doc, cx).await?;
    Ok(expected)
}
/// 打开当前样式的调整项；收起时画廊面板里同样带当前样式的调整项。
async fn open_tune(v: &WeakEntity<CaptureView>, input: Input, cx: &mut AsyncApp) -> Result<()> {
    let (open, inline) = v
        .update(cx, |v, _| {
            (
                v.toolbar_panel == ToolbarPanel::Finish,
                v.ui_positions.borrow().contains_key("selection-style-tune"),
            )
        })
        .map_err(|e| e.to_string())?;
    if !open {
        let id = if inline {
            "selection-style-tune"
        } else {
            "selection-style-menu"
        };
        click(v, input, id, cx).await?;
    }
    panel(v, ToolbarPanel::Finish, cx).await
}
fn mouse(input: Input, message: u32, p: (f32, f32), down: bool) -> Result<()> {
    let x = (p.0 * input.scale).round() as i32;
    let y = (p.1 * input.scale).round() as i32;
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            message,
            WPARAM(
                usize::from(down)
                    | if message == WM_MOUSEMOVE {
                        gpui_smoke::SMOKE_MOVE
                    } else {
                        0
                    },
            ),
            LPARAM((((y as u16 as u32) << 16) | x as u16 as u32) as isize),
        )?;
    }
    Ok(())
}
async fn output(
    v: &WeakEntity<CaptureView>,
    padding: u32,
    doc: &DocumentState,
    cx: &mut AsyncApp,
) -> Result<()> {
    v.update(cx, |v, _| -> Result<()> {
        let before = read(v);
        let crop = v.editor.crop.ok_or("效果导出丢失选区")?;
        let origin = v.finished_origin(crop);
        let image = v.selected_image()?;
        let expected_origin = (
            v.source.bounds.left + crop.x as i32 - padding as i32,
            v.source.bounds.top + crop.y as i32 - padding as i32,
        );
        let center =
            ((padding + crop.height / 2) * image.width + padding + crop.width / 2) as usize * 4;
        if (image.width, image.height) != (300 + padding * 2, 180 + padding * 2)
            || origin != expected_origin
            || v.effective_finish_style().padding().left != padding
            || image.bgra[3] != 0
            || image.bgra[center..center + 4] != [255; 4]
            || !image.bgra.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 255)
        {
            return Err(
                format!("选区效果输出透明度、内容、尺寸或原点错误（padding={padding}）").into(),
            );
        }
        let after = read(v);
        let mut expected = before.clone();
        // selected_image 会保存会话偏好；成功 ACK 只清理已提交的模式意图。
        // 样式、活动模式、精调展开状态与整份文档/选区/历史仍须完全一致。
        expected.intent = None;
        if after != expected || &after.document != doc {
            return Err(
                format!("输出选区效果修改了样式/文档/选区/历史：{before:?} → {after:?}").into(),
            );
        }
        Ok(())
    })
    .map_err(|e| e.to_string())?
}
fn pass(name: &str) {
    println!("{{\"test\":\"{name}\",\"ok\":true}}");
}

async fn verify_output(
    v: &WeakEntity<CaptureView>,
    input: Input,
    window: gpui::AnyWindowHandle,
    mut expected: SelectionFinishStyle,
    doc: &DocumentState,
    cx: &mut AsyncApp,
) -> Result<()> {
    // 描边：圆角 8 + 外侧边框 3。
    output(v, 3, doc, cx).await?;
    open_tune(v, input, cx).await?;
    numeric(v, input, window, "finish-thickness-value", "5", cx).await?;
    expected.border_thickness = 5.;
    check(v, expected, doc, cx).await?;
    output(v, 5, doc, cx).await?;
    // 卡片只有阴影：扩散 16；窗口的阴影从 1px 外侧边框外沿投出：1 + 20。
    look(v, input, FinishPreset::Card, doc, cx).await?;
    output(v, 16, doc, cx).await?;
    look(v, input, FinishPreset::Window, doc, cx).await?;
    output(v, 21, doc, cx).await?;
    pass("native_selection_effect_output_document_stability");
    Ok(())
}

pub(crate) async fn verify_capture(
    v: &WeakEntity<CaptureView>,
    input: Input,
    window: gpui::AnyWindowHandle,
    area: Rect,
    cx: &mut AsyncApp,
) -> Result<()> {
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    v.update(cx, |v, cx| {
        v.editor.cancel_gesture();
        v.editor.cancel_text();
        v.editor.document.clear();
        let mut a = Annotation::new(
            Tool::Rectangle,
            Point {
                x: area.x as f32 + 100.,
                y: area.y as f32 + 100.,
            },
            Style::default(),
        );
        a.update(Point {
            x: area.x as f32 + 300.,
            y: area.y as f32 + 200.,
        });
        let id = v.editor.document.insert(a.clone());
        a.style.color ^= 0x010101;
        v.editor.document.replace(id, a);
        v.editor.document.undo();
        v.editor.select_tool(Tool::Crop);
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
    let initial = state(v, cx).await?;
    if !initial.document.redo {
        return Err("选区效果专项缺少重做种子".into());
    }
    let doc = initial.document;
    if std::env::var("RUSTSHOT_SELECTION_EFFECT_QA_CASE").as_deref() == Ok("output") {
        let expected = look(v, input, FinishPreset::Stroke, &doc, cx).await?;
        return verify_output(v, input, window, expected, &doc, cx).await;
    }
    // 样式直选：卡片=圆角+阴影，描边=圆角+边框，原样=全关，窗口=三者皆有。
    for preset in [
        FinishPreset::Card,
        FinishPreset::Stroke,
        FinishPreset::Raw,
        FinishPreset::Window,
    ] {
        look(v, input, preset, &doc, cx).await?;
    }
    // 悬停只在选区上预览，不提交样式、不动文档。
    let before_hover = state(v, cx).await?;
    let card = look_point(v, input, FinishPreset::Card, cx).await?;
    input.hover((
        (card.0 * input.scale).round() as u32,
        (card.1 * input.scale).round() as u32,
    ))?;
    frame(cx).await;
    v.update(cx, |v, _| -> Result<()> {
        if v.finish_hover != Some(FinishPreset::Card)
            || v.shown_finish_style() != FinishPreset::Card.defaults()
        {
            return Err("悬停“卡片”未在选区上预览它的外观".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    if state(v, cx).await? != before_hover {
        return Err("悬停预览提前写入了样式/历史".into());
    }
    // 滚轮逐个切换当前样式：向上换上一个，向下换下一个，到头不循环。
    for (delta, preset) in [
        (120, FinishPreset::Stroke),
        (-120, FinishPreset::Window),
        (-120, FinishPreset::Window),
    ] {
        input.wheel(card, delta)?;
        frame(cx).await;
        let (current, hover) = v
            .update(cx, |v, _| (v.finish_preset, v.finish_hover))
            .map_err(|e| e.to_string())?;
        if current != preset || hover.is_some() {
            return Err(format!("样式滚轮应切到{preset:?}并结束预览，实际{current:?}").into());
        }
        check(v, preset.defaults(), &doc, cx).await?;
    }
    pass("native_selection_effect_modes_intensity_draft_cancel");

    // “调整”只改当前样式：真实滑条拖到最右后抬键，再移动指针不误改。
    open_tune(v, input, cx).await?;
    let mut expected = FinishPreset::Window.defaults();
    let left = at(v, "finish-thickness-start", cx).await?;
    let right = at(v, "finish-thickness-end", cx).await?;
    let middle = ((left.0 + right.0) * 0.5, left.1);
    mouse(input, WM_LBUTTONDOWN, middle, true)?;
    frame(cx).await;
    mouse(input, WM_MOUSEMOVE, (right.0 + 70., right.1 + 70.), true)?;
    frame(cx).await;
    mouse(input, WM_LBUTTONUP, (right.0 + 70., right.1 + 70.), false)?;
    frame(cx).await;
    expected.border_thickness = 8.;
    check(v, expected, &doc, cx).await?;
    mouse(input, WM_MOUSEMOVE, left, false)?;
    frame(cx).await;
    check(v, expected, &doc, cx).await?;
    for (delta, value) in [(-120, 7.), (120, 8.)] {
        input.wheel(at(v, "finish-thickness", cx).await?, delta)?;
        frame(cx).await;
        expected.border_thickness = value;
        check(v, expected, &doc, cx).await?;
    }
    // 没有效果开关：粗细、扩散设为 0 即关闭并收起附属行，从 0 设值即重新开启。
    numeric(v, input, window, "finish-thickness-value", "0", cx).await?;
    expected.border_enabled = false;
    check(v, expected, &doc, cx).await?;
    numeric(v, input, window, "finish-blur-value", "0", cx).await?;
    expected.shadow_enabled = false;
    check(v, expected, &doc, cx).await?;
    v.update(cx, |v, _| -> Result<()> {
        let positions = v.ui_positions.borrow();
        if v.finish_preset != FinishPreset::Window
            || positions.contains_key("finish-border-inside")
            || positions.contains_key("finish-opacity")
        {
            return Err("参数为 0 后改换了样式，或仍显示边框位置/阴影浓度".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    numeric(v, input, window, "finish-thickness-value", "4", cx).await?;
    expected.border_enabled = true;
    expected.border_thickness = 4.;
    check(v, expected, &doc, cx).await?;
    numeric(v, input, window, "finish-blur-value", "24", cx).await?;
    expected.shadow_enabled = true;
    expected.shadow_blur = 24.;
    check(v, expected, &doc, cx).await?;
    // 恢复默认只把当前样式回到出厂外观。
    click(v, input, "finish-reset", cx).await?;
    check(v, FinishPreset::Window.defaults(), &doc, cx).await?;
    pass("native_selection_effect_slider_wheel_advanced");

    look(v, input, FinishPreset::Stroke, &doc, cx).await?;
    verify_output(v, input, window, FinishPreset::Stroke.defaults(), &doc, cx).await
}
