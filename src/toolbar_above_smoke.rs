//! 两级截图栏的原生布局专项；只保存自有纯白截图上的工具栏局部。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    canvas_source::CanvasSource,
    capture_selection_bar::{SelectionBarDisplay, SelectionBarLayout},
    capture_view::CaptureView,
    editor::Editor,
    export,
    fast_capture::{FrozenDesktop, FrozenScreen},
    geometry::{Rect, ScreenRect},
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    selection_finish::{FinishLooks, FinishPreset},
    selection_finish_view::{look_id, FINISH_POPUP_WIDTH},
    toolbar_layout::{PanelRect, ToolbarLayout, ToolbarPanel},
    Result,
};
use gpui::{App, AsyncApp, Entity, RenderImage, WeakEntity, Window, WindowHandle};
use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, WPARAM},
    Graphics::{Dwm::DwmFlush, Gdi::ClientToScreen},
    UI::WindowsAndMessaging::*,
};

#[derive(Clone)]
struct Painted {
    layout: ToolbarLayout,
    bar: Option<SelectionBarLayout>,
    /// 选区条样式组相对条左缘的 x；外观面板左缘对齐这里，与渲染同一公式。
    style_x: f32,
    properties_visible: bool,
    positions: HashMap<&'static str, (f32, f32)>,
    panel: ToolbarPanel,
}
impl Painted {
    fn position(&self, id: &'static str) -> Result<(f32, f32)> {
        self.positions
            .get(id)
            .copied()
            .ok_or_else(|| format!("未绘制控件：{id}").into())
    }
    fn popup(&self, width: f32, height: f32) -> PanelRect {
        if self.panel == ToolbarPanel::Finish {
            if let Some(bar) = self.bar {
                return bar.style_popup(width, height, bar.panel.x + self.style_x);
            }
        }
        self.layout.popup(
            width,
            height,
            self.positions.get(self.panel.anchor()).copied(),
            true,
        )
    }
}
async fn frame(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(160))
        .await;
}
fn close(a: (f32, f32), b: (f32, f32), scale: f32) -> bool {
    (a.0 - b.0).abs() * scale <= 1. && (a.1 - b.1).abs() * scale <= 1.
}
fn center(rect: PanelRect) -> (f32, f32) {
    (rect.x + rect.width / 2., rect.y + rect.height / 2.)
}
fn contains(rect: PanelRect, p: (f32, f32)) -> bool {
    p.0 >= rect.x && p.0 <= rect.right() && p.1 >= rect.y && p.1 <= rect.bottom()
}
fn overlaps(a: PanelRect, b: PanelRect) -> bool {
    a.x < b.right() && a.right() > b.x && a.y < b.bottom() && a.bottom() > b.y
}
/// 选区条里始终在的入口：尺寸、样式组、更多；样式组收起时只剩一个单按钮。
fn bar_entries(bar: SelectionBarLayout) -> Vec<&'static str> {
    let mut ids = vec!["selection-size-menu", "selection-bar-more"];
    if bar.display == SelectionBarDisplay::Compact {
        ids.push("selection-style-menu");
    } else {
        ids.extend(FinishPreset::ALL.map(|look| look_id(look, false)));
        ids.push("selection-style-tune");
    }
    ids
}
async fn painted(
    owner: WindowHandle<CaptureView>,
    input: Input,
    panel: ToolbarPanel,
    required: &[&'static str],
    popup_size: Option<(f32, f32)>,
    cx: &mut AsyncApp,
) -> Result<Painted> {
    for _ in 0..30 {
        frame(cx).await;
        let value = owner
            .update(cx, |v, w, _| -> Result<Option<Painted>> {
                if v.toolbar_panel != panel {
                    return Ok(None);
                }
                let positions = v.ui_positions.borrow().clone();
                if required.iter().any(|id| !positions.contains_key(id)) {
                    return Ok(None);
                }
                let selected = v.selected()?;
                let layout = v.toolbar_layout(selected, input.scale);
                let tool = v
                    .editor
                    .selected_annotation()
                    .map_or(v.editor.tool, |a| a.tool);
                let properties_visible = tool.draws() || matches!(tool, Tool::Lasso | Tool::Eraser);
                let bar = v.capture_selection_bar_layout(selected, w);
                let style_x = v.selection_bar_style_x(selected, w);
                if !positions
                    .get("primary-toolbar")
                    .is_some_and(|p| close(*p, center(layout.primary), input.scale))
                {
                    return Ok(None);
                }
                if properties_visible {
                    if !positions
                        .get("property-toolbar")
                        .is_some_and(|p| close(*p, center(layout.properties), input.scale))
                    {
                        return Ok(None);
                    }
                } else if positions.contains_key("property-toolbar") {
                    return Ok(None);
                }
                if positions.contains_key("crop") {
                    return Ok(None);
                }
                // 选区条没有整条探针：按布局核对条内各入口都画在条里、不进主栏，条不压两级栏。
                if let Some(bar) = bar {
                    if bar_entries(bar).into_iter().any(|id| {
                        !positions.get(id).is_some_and(|p| {
                            contains(bar.panel, *p) && !contains(layout.primary, *p)
                        })
                    }) || overlaps(bar.panel, layout.primary)
                        || (properties_visible && overlaps(bar.panel, layout.properties))
                    {
                        return Ok(None);
                    }
                } else {
                    return Ok(None);
                }
                let value = Painted {
                    layout,
                    bar,
                    style_x,
                    properties_visible,
                    positions,
                    panel,
                };
                if let Some((width, height)) = popup_size {
                    if !value
                        .positions
                        .get("toolbar-popup")
                        .is_some_and(|p| close(*p, center(value.popup(width, height)), input.scale))
                    {
                        return Ok(None);
                    }
                }
                Ok(Some(value))
            })
            .map_err(|e| e.to_string())??;
        if let Some(value) = value {
            return Ok(value);
        }
    }
    // 附上缺失控件与二级栏实绘中心，便于区分“控件没画出来”和“估算位置不符”。
    let detail = owner
        .update(cx, |v, w, _| -> Result<String> {
            let positions = v.ui_positions.borrow().clone();
            let missing: Vec<_> = required
                .iter()
                .filter(|id| !positions.contains_key(*id))
                .collect();
            let selected = v.selected()?;
            let layout = v.toolbar_layout(selected, input.scale);
            let bar = v.capture_selection_bar_layout(selected, w);
            let entries: Vec<_> = bar
                .map(bar_entries)
                .unwrap_or_default()
                .into_iter()
                .map(|id| (id, positions.get(id).copied()))
                .collect();
            Ok(format!(
                "缺失 {missing:?}，二级栏实绘 {:?}，估算 {:?}；主栏实绘 {:?}，估算 {:?}；选区条估算 {:?}，条内入口 {entries:?}；弹窗实绘 {:?}；crop {}",
                positions.get("property-toolbar"),
                layout.properties,
                positions.get("primary-toolbar"),
                layout.primary,
                bar.map(|b| (b.panel, b.display)),
                positions.get("toolbar-popup"),
                positions.contains_key("crop")
            ))
        })
        .map_err(|e| e.to_string())??;
    Err(format!("两级栏布局/实际绘制未收敛：{panel:?}，控件 {required:?}；{detail}").into())
}
async fn click(
    view: &WeakEntity<CaptureView>,
    input: Input,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    let p = view
        .update(cx, |v, _| v.ui_positions.borrow().get(id).copied())
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("点击目标未绘制：{id}"))?;
    input.click(p);
    Ok(())
}
fn stable(before: &Painted, after: &Painted, scale: f32) -> Result<()> {
    if before.layout.primary != after.layout.primary
        || !close(
            before.position("primary-toolbar")?,
            after.position("primary-toolbar")?,
            scale,
        )
    {
        return Err("切换属性/选择/弹窗改变一级栏位置或尺寸".into());
    }
    Ok(())
}
fn side(value: &Painted, monitor: Rect, above: bool, scale: f32) -> Result<()> {
    let m = PanelRect {
        x: monitor.x as f32 / scale,
        y: monitor.y as f32 / scale,
        width: monitor.width as f32 / scale,
        height: monitor.height as f32 / scale,
    };
    if value.layout.properties_above != above {
        return Err("工具栏侧向与手动锚点不符".into());
    }
    let main = value.layout.primary;
    let properties = value.layout.properties;
    if (above && properties.bottom() > main.y - 7.) || (!above && properties.y < main.bottom() + 7.)
    {
        return Err("二级栏没有在一级栏外侧保持间距".into());
    }
    for rect in [main, properties] {
        if !contains(m, (rect.x, rect.y)) || !contains(m, (rect.right(), rect.bottom())) {
            return Err(format!("两级栏超出显示器：{rect:?}").into());
        }
    }
    Ok(())
}
fn rows(value: &Painted, basics: &[&'static str], above: bool) -> Result<()> {
    // 对象段与样式同排按组换行：可在同一行，但不能排到比样式更靠近选区的一侧。
    let transforms = ["angle-menu", "arrange-menu"];
    for basic in basics {
        let b = value.position(basic)?;
        if !contains(value.layout.properties, b) {
            return Err(format!("基础属性溢出二级栏：{basic}").into());
        }
        for transform in transforms {
            let t = value.position(transform)?;
            if !contains(value.layout.properties, t)
                || (above && t.1 > b.1 + 4.)
                || (!above && t.1 < b.1 - 4.)
            {
                return Err(format!(
                    "基础属性未靠近选区/变换行未向外：{basic}={b:?}, {transform}={t:?}"
                )
                .into());
            }
        }
    }
    Ok(())
}
fn popup_side(value: &Painted, size: (f32, f32), above: bool) -> Result<PanelRect> {
    let rect = value.popup(size.0, size.1);
    if (above && rect.bottom() > value.layout.properties.y - 7.)
        || (!above && rect.y < value.layout.properties.bottom() + 7.)
    {
        return Err(format!("弹窗没有朝整组外侧展开：{rect:?}").into());
    }
    Ok(rect)
}
/// 外观面板已换到 `look` 后，按它实际展开的行数定高再核对实绘位置；高度与渲染同一公式。
async fn finish_painted(
    owner: WindowHandle<CaptureView>,
    input: Input,
    look: FinishPreset,
    required: &[&'static str],
    cx: &mut AsyncApp,
) -> Result<(Painted, PanelRect)> {
    for _ in 0..30 {
        let height = owner
            .update(cx, |v, _, _| {
                (v.toolbar_panel == ToolbarPanel::Finish && v.finish_preset == look)
                    .then(|| v.finish_popup_height())
            })
            .map_err(|e| e.to_string())?;
        if let Some(height) = height {
            let size = (FINISH_POPUP_WIDTH, height);
            let value =
                painted(owner, input, ToolbarPanel::Finish, required, Some(size), cx).await?;
            let popup = value.popup(size.0, size.1);
            return Ok((value, popup));
        }
        frame(cx).await;
    }
    Err(format!("外观面板未打开或未换到{look:?}").into())
}
fn bar_popup(value: &Painted, rect: PanelRect, monitor: Rect, scale: f32) -> Result<()> {
    let bar = value.bar.ok_or("截图选区条未绘制")?;
    let bounds = PanelRect {
        x: monitor.x as f32 / scale,
        y: monitor.y as f32 / scale,
        width: monitor.width as f32 / scale,
        height: monitor.height as f32 / scale,
    };
    if !contains(bounds, (rect.x, rect.y))
        || !contains(bounds, (rect.right(), rect.bottom()))
        || overlaps(rect, bar.panel)
    {
        return Err(format!("外观面板越界或覆盖选区条：{rect:?}，条 {:?}", bar.panel).into());
    }
    Ok(())
}
/// 面板变高时选区条不动、面板横向不动；在条上方或下方时，贴着条的那条边不动。
fn grows_outwards(
    short: &Painted,
    low: PanelRect,
    tall: &Painted,
    high: PanelRect,
    scale: f32,
) -> Result<()> {
    let bar = short.bar.ok_or("截图选区条未绘制")?.panel;
    let near = |a: f32, b: f32| (a - b).abs() * scale <= 1.;
    let edge = if low.bottom() <= bar.y + 0.5 {
        near(low.bottom(), high.bottom())
    } else if low.y + 0.5 >= bar.bottom() {
        near(low.y, high.y)
    } else {
        true
    };
    if tall.bar.map(|bar| bar.panel) != Some(bar)
        || !near(low.x, high.x)
        || !near(low.width, high.width)
        || high.height <= low.height + 1.
        || !edge
    {
        return Err(format!(
            "外观面板长高时移动了选区条或横向位置，或朝选区条一侧展开：{low:?} → {high:?}，条 {bar:?}"
        )
        .into());
    }
    Ok(())
}
fn install(v: &mut CaptureView, scale: f32) -> Result<Rect> {
    let bounds = v.source.bounds;
    let current = (0..v.source.screen_count())
        .find(|&i| {
            let r = v.source.screen_rect(i);
            r.width as f32 / scale >= 900. && r.height as f32 / scale >= 780.
        })
        .ok_or("专项需要至少900×780逻辑像素显示器，以验证440高弹窗向外完整展开")?;
    let monitor = v.source.screen_rect(current);
    let screens = (0..v.source.screen_count())
        .map(|i| {
            let r = v.source.screen_rect(i);
            FrozenScreen {
                rect: ScreenRect {
                    left: bounds.left + r.x as i32,
                    top: bounds.top + r.y as i32,
                    width: r.width,
                    height: r.height,
                },
                preview: Arc::new(RenderImage::new(vec![image::Frame::new(
                    image::RgbaImage::from_pixel(r.width, r.height, image::Rgba([255; 4])),
                )])),
            }
        })
        .collect();
    let (source, previews) = CanvasSource::frozen_desktop(FrozenDesktop {
        screens,
        bounds,
        current,
        capture_ms: 0.,
    });
    v.source = source;
    v.previews = previews;
    v.image_canvas = None;
    v.toolbar_config = Default::default();
    v.toolbar_monitor = current;
    v.include_cursor = false;
    v.cursor = None;
    v.cursor_preview = None;
    v.magnifier = false;
    v.guides = false;
    v.show_guides = false;
    v.window_targets.clear();
    v.hover_window = None;
    v.finish_style = Default::default();
    // 从原样与各样式出厂外观开始，面板高度不受此前会话记住的样式影响。
    v.finish_preset = FinishPreset::Raw;
    v.finish_looks = FinishLooks::default();
    v.finish_hover = None;
    v.finish_preview = None;
    v.finish_advanced = false;
    v.raster_previews.clear();
    Ok(monitor)
}
fn setup(v: &mut CaptureView, monitor: Rect, scale: f32, above: bool, count: usize) -> Rect {
    let mx = monitor.x as f32 / scale;
    let my = monitor.y as f32 / scale;
    let width = monitor.width as f32 / scale;
    let height = monitor.height as f32 / scale;
    let py = my + if above { height - 185. } else { 108. };
    let crop_y = if above { py + 70. } else { my + 14. };
    let crop = Rect {
        x: ((mx + 60.) * scale).round() as u32,
        y: (crop_y * scale).round() as u32,
        width: (640. * scale).round() as u32,
        height: (64. * scale).round() as u32,
    };
    v.editor = Editor::new(v.source.full(), Some(crop));
    for i in 0..count {
        let style = Style {
            radius: 8.,
            width: 2.,
            ..Default::default()
        };
        let mut annotation = Annotation::new(
            Tool::Rectangle,
            Point {
                x: crop.x as f32 + (30. + i as f32 * 180.) * scale,
                y: crop.y as f32 + 10. * scale,
            },
            style,
        );
        annotation.update(Point {
            x: crop.x as f32 + (170. + i as f32 * 180.) * scale,
            y: crop.y as f32 + 54. * scale,
        });
        v.editor.document.insert(annotation);
    }
    if count > 0 {
        v.editor.select_all_objects();
    } else {
        v.editor.select_tool(Tool::Rectangle);
    }
    v.toolbar_position = Some((
        mx + (width - ToolbarLayout::primary_width_with_tools(&v.toolbar_config.slots())) / 2.,
        py,
    ));
    v.toolbar_panel = ToolbarPanel::None;
    v.ui_positions.borrow_mut().clear();
    v.pointer = (
        (crop.x as f32 + 600. * scale) as i32,
        (crop.y as f32 + 40. * scale) as i32,
    );
    v.pointer_sample = Point::from_tuple(v.pointer);
    crop
}
fn owned(input: Input, rect: Rect) -> Result<()> {
    unsafe {
        let h = HWND(input.handle as *mut _);
        if !IsWindowVisible(h).as_bool() || GetForegroundWindow() != h {
            return Err("用户切换了前台窗口；停止原生采样".into());
        }
        for (x, y) in [
            (rect.x, rect.y),
            (rect.x + rect.width - 1, rect.y),
            (rect.x, rect.y + rect.height - 1),
            (rect.x + rect.width - 1, rect.y + rect.height - 1),
            (rect.x + rect.width / 2, rect.y + rect.height / 2),
        ] {
            let mut p = POINT {
                x: x as i32,
                y: y as i32,
            };
            ClientToScreen(h, &mut p).ok()?;
            if WindowFromPoint(p) != h {
                return Err("采样区域被其他窗口覆盖；不保存桌面图片".into());
            }
        }
    }
    Ok(())
}
fn save_native(
    input: Input,
    value: &Painted,
    extra: Option<PanelRect>,
    folder: &Path,
    name: &str,
) -> Result<()> {
    let mut rects = vec![value.layout.primary];
    if value.properties_visible {
        rects.push(value.layout.properties);
    }
    if let Some(bar) = value.bar {
        rects.push(bar.panel);
    }
    if let Some(rect) = extra {
        rects.push(rect);
    }
    let left = rects.iter().map(|r| r.x).fold(f32::INFINITY, f32::min);
    let top = rects.iter().map(|r| r.y).fold(f32::INFINITY, f32::min);
    let right = rects.iter().map(|r| r.right()).fold(0., f32::max);
    let bottom = rects.iter().map(|r| r.bottom()).fold(0., f32::max);
    let region = Rect {
        x: (left * input.scale).floor() as u32,
        y: (top * input.scale).floor() as u32,
        width: (right * input.scale).ceil() as u32 - (left * input.scale).floor() as u32,
        height: (bottom * input.scale).ceil() as u32 - (top * input.scale).floor() as u32,
    };
    owned(input, region)?;
    unsafe {
        DwmFlush()?;
    }
    let image = crate::spotlight_smoke::native_owned_region(region)?;
    owned(input, region)?;
    // 纯白底图的选区外为约136灰，深色栏应真实覆盖边缘；不能只凭模型判定可见。
    for rect in rects {
        let x = ((rect.right() - 4.) * input.scale).round() as u32 - region.x;
        let y = ((rect.y + rect.height / 2.) * input.scale).round() as u32 - region.y;
        let index = ((y * image.width + x) * 4) as usize;
        let pixel = &image.bgra[index..index + 4];
        if pixel[..3].iter().any(|c| *c > 80) || pixel[3] != 255 {
            return Err(format!("原生工具栏边缘未覆盖预期位置：{rect:?}, BGRA={pixel:?}").into());
        }
    }
    export::save_png(image, &folder.join(format!("{name}.png")))
}
fn pass(cases: &Rc<RefCell<Vec<&'static str>>>, name: &'static str) {
    cases.borrow_mut().push(name);
    println!("{{\"test\":\"{name}\",\"ok\":true}}");
}
async fn verify(
    view: WeakEntity<CaptureView>,
    owner: WindowHandle<CaptureView>,
    input: Input,
    folder: &Path,
    cases: &Rc<RefCell<Vec<&'static str>>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    std::fs::create_dir_all(folder)?;
    let monitor = view
        .update(cx, |v, cx| {
            let result = install(v, input.scale);
            if let Ok(m) = &result {
                setup(v, *m, input.scale, true, 1);
            }
            cx.notify();
            result
        })
        .map_err(|e| e.to_string())??;
    let single = painted(
        owner,
        input,
        ToolbarPanel::None,
        &["width-menu", "radius-menu", "angle-menu"],
        None,
        cx,
    )
    .await?;
    side(&single, monitor, true, input.scale)?;
    rows(&single, &["width-menu", "radius-menu"], true)?;
    save_native(input, &single, None, folder, "above-single")?;
    view.update(cx, |v, cx| {
        setup(v, monitor, input.scale, true, 2);
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    let multi = painted(
        owner,
        input,
        ToolbarPanel::None,
        &["radius-menu", "arrange-menu", "angle-menu"],
        None,
        cx,
    )
    .await?;
    stable(&single, &multi, input.scale)?;
    rows(&multi, &["radius-menu"], true)?;
    if (single.layout.properties.bottom() - multi.layout.properties.bottom()).abs() * input.scale
        > 1.
    {
        return Err("单选/多选改变上方属性栏靠一级的底边".into());
    }
    save_native(input, &multi, None, folder, "above-multi")?;
    click(&view, input, "radius-menu", cx).await?;
    let radius = painted(
        owner,
        input,
        ToolbarPanel::Radius,
        &["parameter-cancel", "toolbar-popup"],
        Some((300., 232.)),
        cx,
    )
    .await?;
    stable(&single, &radius, input.scale)?;
    save_native(
        input,
        &radius,
        Some(popup_side(&radius, (300., 232.), true)?),
        folder,
        "above-radius",
    )?;
    click(&view, input, "parameter-cancel", cx).await?;
    painted(owner, input, ToolbarPanel::None, &["radius-menu"], None, cx).await?;
    pass(
        cases,
        "toolbar_above_native_single_multi_rows_and_radius_popup",
    );

    click(&view, input, "edit", cx).await?;
    let crop = painted(
        owner,
        input,
        ToolbarPanel::None,
        &["selection-size-menu"],
        None,
        cx,
    )
    .await?;
    stable(&single, &crop, input.scale)?;
    let compact = crop.bar.ok_or("截图选区条未绘制")?.display == SelectionBarDisplay::Compact;
    // 原样的外观面板只有一行提示；换成“窗口”多出阴影与边框各行，面板须朝离开选区条的一侧长高。
    // 条收起时单按钮打开画廊，样式在画廊大缩略图里换。
    click(
        &view,
        input,
        if compact {
            "selection-style-menu"
        } else {
            "selection-style-tune"
        },
        cx,
    )
    .await?;
    let (basic, basic_popup) =
        finish_painted(owner, input, FinishPreset::Raw, &["finish-popup"], cx).await?;
    bar_popup(&basic, basic_popup, monitor, input.scale)?;
    stable(&crop, &basic, input.scale)?;
    save_native(
        input,
        &basic,
        Some(basic_popup),
        folder,
        "above-finish-basic",
    )?;
    click(&view, input, look_id(FinishPreset::Window, compact), cx).await?;
    let (advanced, advanced_popup) = finish_painted(
        owner,
        input,
        FinishPreset::Window,
        &[
            "finish-popup",
            "finish-opacity",
            "finish-thickness",
            "finish-border-inside",
            "finish-reset",
        ],
        cx,
    )
    .await?;
    bar_popup(&advanced, advanced_popup, monitor, input.scale)?;
    stable(&basic, &advanced, input.scale)?;
    grows_outwards(&basic, basic_popup, &advanced, advanced_popup, input.scale)?;
    save_native(
        input,
        &advanced,
        Some(advanced_popup),
        folder,
        "above-finish-advanced",
    )?;
    click(&view, input, look_id(FinishPreset::Raw, compact), cx).await?;
    let (collapsed, collapsed_popup) =
        finish_painted(owner, input, FinishPreset::Raw, &["finish-popup"], cx).await?;
    if collapsed_popup != basic_popup {
        return Err("换回原样没有恢复外观面板的位置与高度".into());
    }
    stable(&basic, &collapsed, input.scale)?;
    if collapsed.bar.map(|bar| bar.panel) != basic.bar.map(|bar| bar.panel) {
        return Err("换样式改变了选区条位置或尺寸".into());
    }
    input.key(0x1b);
    painted(
        owner,
        input,
        ToolbarPanel::None,
        &["selection-size-menu"],
        None,
        cx,
    )
    .await?;
    pass(cases, "toolbar_above_native_finish_advanced_grows_outwards");

    view.update(cx, |v, cx| {
        setup(v, monitor, input.scale, false, 1);
        cx.notify();
    })
    .map_err(|e| e.to_string())?;
    let below = painted(
        owner,
        input,
        ToolbarPanel::None,
        &["width-menu", "radius-menu", "angle-menu"],
        None,
        cx,
    )
    .await?;
    side(&below, monitor, false, input.scale)?;
    rows(&below, &["width-menu", "radius-menu"], false)?;
    save_native(input, &below, None, folder, "below-single")?;
    click(&view, input, "radius-menu", cx).await?;
    let below_popup = painted(
        owner,
        input,
        ToolbarPanel::Radius,
        &["parameter-cancel", "toolbar-popup"],
        Some((300., 232.)),
        cx,
    )
    .await?;
    stable(&below, &below_popup, input.scale)?;
    save_native(
        input,
        &below_popup,
        Some(popup_side(&below_popup, (300., 232.), false)?),
        folder,
        "below-radius",
    )?;
    click(&view, input, "parameter-cancel", cx).await?;
    painted(owner, input, ToolbarPanel::None, &["radius-menu"], None, cx).await?;
    for (id, property) in [
        ("pen", "width-menu"),
        ("text", "font-menu"),
        ("rectangle", "radius-menu"),
    ] {
        click(&view, input, id, cx).await?;
        let after = painted(owner, input, ToolbarPanel::None, &[property], None, cx).await?;
        stable(&below, &after, input.scale)?;
        side(&after, monitor, false, input.scale)?;
    }
    pass(
        cases,
        "toolbar_below_native_rows_popup_and_primary_stability",
    );
    Ok(())
}
pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    *errors.borrow_mut() = Some("上方两级工具栏专项未执行完成".into());
    let weak = view.downgrade();
    // 选区条布局与样式组位置要用窗口文字度量，按窗口句柄读取。
    let owner = window
        .window_handle()
        .downcast::<CaptureView>()
        .expect("capture window");
    let input = Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    };
    let folder = std::env::var_os("RUSTSHOT_TOOLBAR_ABOVE_QA_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/qa/toolbar-above"));
    window.on_next_frame(move |_, cx| {
        cx.spawn(async move |cx| {
            let cases = Rc::new(RefCell::new(Vec::new()));
            let result = verify(weak, owner, input, &folder, &cases, cx).await;
            let mut error = result.err().map(|e| e.to_string());
            let report = serde_json::json!({"ok":error.is_none(), "cases":*cases.borrow(), "error":error, "scale":input.scale});
            let written = (|| -> Result<()> {
                std::fs::create_dir_all(&folder)?;
                std::fs::write(folder.join("summary.json"), serde_json::to_vec_pretty(&report)?)?;
                Ok(())
            })();
            if let Err(e) = written { error = Some(format!("专项报告写入失败：{e}；原始错误：{error:?}")); }
            *errors.borrow_mut() = error;
            let _ = cx.update(|cx| cx.quit());
            unsafe { let _ = PostMessageW(Some(HWND(input.handle as *mut _)), WM_NULL, WPARAM(0), LPARAM(0)); }
        }).detach();
    });
}
