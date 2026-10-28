//! 贴图缩放原生验收：仅自有合成图，直接读取 GPUI 帧缓冲，不读取用户桌面。
use crate::{
    annotation::{Annotation, Point, Style, Tool},
    capture::Image,
    capture_view::Action,
    document::Object,
    export,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_model::{PinState, Placement},
    pin_native as native,
    pin_toolbar_layout::PinToolbarLayout,
    pin_view::{PinAction, PinView},
    viewport::Viewport,
    Result,
};
use gpui::{App, AsyncApp, WindowHandle};
use std::{cell::RefCell, path::Path, rc::Rc, time::Duration};
use windows::Win32::{
    Foundation::*,
    UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
};

const FOLDER: &str = "target/qa/pin-zoom";
const WIDTH: u32 = 480;
const HEIGHT: u32 = 320;
const COLORS: [(&str, u32); 5] = [
    ("rectangle", 0xed354b),
    ("arrow", 0x39cb63),
    ("pen", 0x28a6ed),
    ("text", 0xb255e0),
    ("spotlight-border", 0xff8700),
];
fn ensure(ok: bool, label: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(label.into())
    }
}
fn p(x: f32, y: f32) -> Point {
    Point { x, y }
}
fn fixture() -> Image {
    let mut image = Image {
        width: WIDTH,
        height: HEIGHT,
        bgra: vec![255; (WIDTH * HEIGHT * 4) as usize],
        readback_ms: 0.,
    };
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let color = if (320..464).contains(&x) && (144..256).contains(&y) {
                if (x / 4 + y / 4) % 2 == 0 {
                    [40, 180, 240]
                } else {
                    [220, 80, 30]
                }
            } else if (32..144).contains(&x) && (260..296).contains(&y) {
                [120, 180, 80]
            } else if (208..312).contains(&x) && (260..296).contains(&y) {
                [230, 200, 90]
            } else {
                [255, 255, 255]
            };
            let at = ((y * WIDTH + x) * 4) as usize;
            image.bgra[at..at + 3].copy_from_slice(&color);
        }
    }
    image
}
fn annotations() -> Vec<Annotation> {
    let rect = |tool, start, end, style| {
        let mut a = Annotation::new(tool, start, style);
        a.update(end);
        a
    };
    let spot = rect(
        Tool::Spotlight,
        p(340., 36.),
        p(448., 112.),
        Style {
            color: COLORS[4].1,
            width: 6.,
            radius: 16.,
            opacity: 0.4,
            ..Default::default()
        },
    );
    let mosaic = rect(
        Tool::Mosaic,
        p(336., 160.),
        p(448., 240.),
        Style {
            block_size: 16,
            ..Default::default()
        },
    );
    let rectangle = rect(
        Tool::Rectangle,
        p(32., 40.),
        p(152., 112.),
        Style {
            color: COLORS[0].1,
            width: 8.,
            radius: 12.,
            ..Default::default()
        },
    );
    let arrow = rect(
        Tool::Arrow,
        p(208., 60.),
        p(304., 100.),
        Style {
            color: COLORS[1].1,
            width: 8.,
            ..Default::default()
        },
    );
    let mut pen = Annotation::new(
        Tool::Pen,
        p(32., 176.),
        Style {
            color: COLORS[2].1,
            width: 8.,
            ..Default::default()
        },
    );
    for point in [p(64., 140.), p(96., 196.), p(136., 152.), p(164., 180.)] {
        pen.update(point);
    }
    let mut text = rect(
        Tool::Text,
        p(208., 152.),
        p(312., 208.),
        Style {
            color: COLORS[3].1,
            font_size: 36.,
            bold: true,
            ..Default::default()
        },
    );
    text.text = "GPUI".into();
    vec![spot, mosaic, rectangle, arrow, pen, text]
}
async fn frame(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(70))
        .await;
}
fn owned(input: Input) -> Result<()> {
    unsafe {
        let h = HWND(input.handle as *mut _);
        let mut pid = 0;
        GetWindowThreadProcessId(h, Some(&mut pid));
        ensure(
            IsWindow(Some(h)).as_bool() && pid == std::process::id(),
            "缩放 QA 输入目标不是本进程自有贴图",
        )
    }
}
fn input(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Input> {
    let input = pin
        .update(cx, |_, w, _| Input {
            handle: hwnd(w).0 as usize,
            scale: w.scale_factor(),
        })
        .map_err(|e| e.to_string())?;
    owned(input)?;
    Ok(input)
}
fn action(pin: WindowHandle<PinView>, action: PinAction, cx: &mut AsyncApp) -> Result<()> {
    pin.update(cx, |v, w, cx| v.perform(action, w, cx))
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn tool(pin: WindowHandle<PinView>, tool: Tool, cx: &mut AsyncApp) -> Result<()> {
    pin.update(cx, |v, w, cx| v.annotation(Action::Tool(tool), w, cx))
        .map_err(|e| e.to_string())?;
    Ok(())
}
#[derive(Clone, Debug)]
struct DocumentState {
    objects: Vec<Object>,
    revision: u64,
    edit_revision: u64,
    undo: bool,
    redo: bool,
}
fn document(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<DocumentState> {
    pin.update(cx, |v, _, cx| -> Result<_> {
        let canvas = v.canvas.as_ref().ok_or("缩放 QA 缺少标注画布")?.read(cx);
        Ok(DocumentState {
            objects: canvas.editor.document.items().to_vec(),
            revision: canvas.editor.document.revision(),
            edit_revision: canvas.editor.document.edit_revision(),
            undo: canvas.editor.document.can_undo(),
            redo: canvas.editor.document.can_redo(),
        })
    })
    .map_err(|e| e.to_string())?
}
fn unchanged(before: &DocumentState, after: &DocumentState, label: &str) -> Result<()> {
    ensure(
        before.objects == after.objects
            && before.revision == after.revision
            && before.edit_revision == after.edit_revision
            && before.undo == after.undo
            && before.redo == after.redo,
        label,
    )
}
fn output(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Image> {
    pin.update(cx, |v, _, cx| v.output(cx))
        .map_err(|e| e.to_string())?
}
fn same_image(actual: &Image, expected: &Image, label: &str) -> Result<()> {
    ensure(
        actual.width == expected.width
            && actual.height == expected.height
            && actual.bgra == expected.bgra,
        label,
    )
}
fn copy_image(image: &Image) -> Image {
    Image {
        width: image.width,
        height: image.height,
        bgra: image.bgra.clone(),
        readback_ms: image.readback_ms,
    }
}
#[derive(Clone, Debug)]
struct Meta {
    placement: Placement,
    physical: (u32, u32),
    dpi: f32,
    native_dpi: u32,
    zoom: f32,
    viewport: Viewport,
}
fn meta(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Meta> {
    pin.update(cx, |v, w, cx| -> Result<_> {
        let canvas = v.canvas.as_ref().ok_or("缩放 QA 缺少画布")?.read(cx);
        let viewport = canvas
            .image_canvas
            .as_ref()
            .ok_or("缩放 QA 缺少视口")?
            .viewport;
        let size = w.viewport_size();
        let dpi = w.scale_factor();
        let mut client = RECT::default();
        unsafe {
            GetClientRect(hwnd(w), &mut client)?;
        }
        let physical = (
            (f32::from(size.width) * dpi).round() as u32,
            (f32::from(size.height) * dpi).round() as u32,
        );
        ensure(
            (client.right - client.left, client.bottom - client.top)
                == (physical.0 as i32, physical.1 as i32),
            "GPUI 物理 viewport 与 Win32 客户区不一致",
        )?;
        ensure(
            v.original_size == (WIDTH, HEIGHT)
                && v.image_size == (WIDTH, HEIGHT)
                && v.state.adjustments == Default::default(),
            "缩放修改了底图尺寸或图像处理参数",
        )?;
        Ok(Meta {
            placement: native::rect(hwnd(w))?,
            physical,
            dpi,
            native_dpi: unsafe { GetDpiForWindow(hwnd(w)) },
            zoom: v.state.zoom,
            viewport,
        })
    })
    .map_err(|e| e.to_string())?
}
fn meta_log(meta: &Meta) -> serde_json::Value {
    serde_json::json!({
        "window":[meta.placement.x,meta.placement.y,meta.placement.width,meta.placement.height],
        "physical_client":[meta.physical.0,meta.physical.1],
        "gpui_dpi_scale":meta.dpi,"native_dpi":meta.native_dpi,
        "requested_zoom":meta.zoom,"viewport_zoom":meta.viewport.zoom,
        "viewport_offset":[meta.viewport.offset.x,meta.viewport.offset.y]
    })
}
async fn settle(pin: WindowHandle<PinView>, zoom: f32, cx: &mut AsyncApp) -> Result<Meta> {
    let size = (
        (WIDTH as f32 * zoom).round() as u32,
        (HEIGHT as f32 * zoom).round() as u32,
    );
    let mut last = None;
    for _ in 0..50 {
        frame(cx).await;
        let value = meta(pin, cx)?;
        let ok = value.physical == size
            && (value.placement.width, value.placement.height) == size
            && (value.zoom - zoom).abs() < 0.001
            && (value.viewport.zoom - zoom).abs() < 0.001
            && value.viewport.offset.distance(p(0., 0.)) < 0.01
            && (value.native_dpi as f32 / 96. - value.dpi).abs() < 0.01;
        if ok {
            return Ok(value);
        }
        last = Some(meta_log(&value));
    }
    Err(format!("原生缩放 {zoom} 的尺寸/DPI/画布变换未收敛：{last:?}").into())
}
async fn framebuffer(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Image> {
    let size = meta(pin, cx)?.physical;
    pin.update(cx, |v, w, cx| {
        // 只隐藏自有夹具的悬停按钮/辅助指针，避免把它们算进标注颜色包围盒。
        // 视口、底图、文档和原生位置均由产品逻辑提供。
        v.hovered = false;
        if let Some(canvas) = &v.canvas {
            canvas.update(cx, |v, cx| {
                v.pointer = (-1000, -1000);
                v.pointer_sample = p(-1000., -1000.);
                cx.notify();
            });
        }
        w.request_frame_capture([0, 0, size.0, size.1])
    })
    .map_err(|e| e.to_string())??;
    for _ in 0..60 {
        frame(cx).await;
        if let Some(bytes) = pin
            .update(cx, |_, w, _| w.take_frame_capture())
            .map_err(|e| e.to_string())?
        {
            let image = Image {
                width: size.0,
                height: size.1,
                bgra: bytes.map_err(|e| e.to_string())?,
                readback_ms: 0.,
            };
            ensure(
                image.bgra.len() == (size.0 * size.1 * 4) as usize,
                "贴图 GPU 帧缓冲数据长度无效",
            )?;
            // 随后像素断言失败时，仍保留当时的自有 GPU 帧供定位。
            export::save_png(
                copy_image(&image),
                &Path::new(FOLDER).join("last-frame.png"),
            )?;
            return Ok(image);
        }
    }
    Err("贴图 GPU 帧缓冲读取超时".into())
}
fn pixel(image: &Image, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * image.width + x) * 4) as usize;
    [image.bgra[i], image.bgra[i + 1], image.bgra[i + 2]]
}
fn color_bounds(image: &Image, color: u32) -> Result<([u32; 4], usize)> {
    let expected = [color as u8, (color >> 8) as u8, (color >> 16) as u8];
    let mut bounds = [image.width, image.height, 0, 0];
    let mut count = 0;
    for y in 0..image.height {
        for x in 0..image.width {
            if pixel(image, x, y)
                .iter()
                .zip(expected)
                .all(|(&a, b)| a.abs_diff(b) <= 48)
            {
                bounds[0] = bounds[0].min(x);
                bounds[1] = bounds[1].min(y);
                bounds[2] = bounds[2].max(x + 1);
                bounds[3] = bounds[3].max(y + 1);
                count += 1;
            }
        }
    }
    ensure(count > 0, "GPU 缩放帧缺少某类标注的可见像素")?;
    Ok((bounds, count))
}
fn image_checks(native: &Image, exported: &Image, viewport: Viewport) -> Result<serde_json::Value> {
    let mut colors = Vec::new();
    for (label, color) in COLORS {
        let (source, source_count) = color_bounds(exported, color)?;
        let (observed, count) = color_bounds(native, color)?;
        let top = viewport.window_point(p(source[0] as f32, source[1] as f32));
        let bottom = viewport.window_point(p(source[2] as f32, source[3] as f32));
        let expected = [top.x, top.y, bottom.x, bottom.y];
        let max_error = observed
            .iter()
            .zip(expected)
            .map(|(&a, b)| (a as f32 - b).abs())
            .fold(0_f32, f32::max);
        ensure(
            max_error <= 3.,
            &format!("{label} 与底图缩放不同步：{observed:?} / {expected:?}"),
        )?;
        ensure(
            count as f32 >= (source_count as f32 * viewport.zoom.powi(2) * 0.06).max(2.),
            &format!("{label} 缩放后只剩少量孤立像素"),
        )?;
        colors.push(serde_json::json!({"tool":label,"source_bounds":source,
            "expected_bounds":expected,"observed_bounds":observed,
            "max_error_physical_px":max_error,"visible_pixels":count}));
    }
    let mut probes = Vec::new();
    for (label, x, y) in [
        ("base-marker-a", 80, 278),
        ("base-marker-b", 264, 278),
        ("base-background", 192, 272),
        ("spotlight-hole", 394, 76),
        ("spotlight-outside", 326, 76),
        ("mosaic-a", 344, 168),
        ("mosaic-b", 360, 184),
        ("mosaic-c", 392, 216),
        ("mosaic-d", 440, 232),
    ] {
        let mapped = viewport.window_point(p(x as f32 + 0.5, y as f32 + 0.5));
        let (nx, ny) = (mapped.x.floor() as u32, mapped.y.floor() as u32);
        ensure(
            nx < native.width && ny < native.height,
            "图像探针映射到窗口外",
        )?;
        let expected = pixel(exported, x, y);
        let actual = pixel(native, nx, ny);
        let error = actual
            .iter()
            .zip(expected)
            .map(|(&a, b)| a.abs_diff(b))
            .max()
            .unwrap_or(0);
        ensure(
            error <= 6,
            &format!("{label} GPU 实际像素错误：{actual:?} / {expected:?}"),
        )?;
        probes.push(
            serde_json::json!({"probe":label,"image":[x,y],"physical":[nx,ny],
            "expected_bgr":expected,"observed_bgr":actual,"max_channel_error":error}),
        );
    }
    Ok(serde_json::json!({"colors":colors,"probes":probes,"framebuffer":true}))
}
#[derive(Clone, Debug)]
struct Bars {
    primary: usize,
    secondary: usize,
    primary_rect: Placement,
    secondary_rect: Placement,
}
fn bars(pin: WindowHandle<PinView>, cx: &mut AsyncApp) -> Result<Bars> {
    let primary = pin
        .update(cx, |v, _, _| v.toolbar)
        .map_err(|e| e.to_string())?
        .ok_or("缩放后一级工具栏消失")?;
    let (handle, primary_rect, secondary) = primary
        .update(cx, |v, w, _| -> Result<_> {
            Ok((
                hwnd(w).0 as usize,
                native::rect(hwnd(w))?,
                v.properties.ok_or("缩放后二级工具栏消失")?,
            ))
        })
        .map_err(|e| e.to_string())??;
    let (secondary, secondary_rect) = secondary
        .update(cx, |_, w, _| -> Result<_> {
            Ok((hwnd(w).0 as usize, native::rect(hwnd(w))?))
        })
        .map_err(|e| e.to_string())??;
    Ok(Bars {
        primary: handle,
        secondary,
        primary_rect,
        secondary_rect,
    })
}
async fn toolbar_check(
    pin: WindowHandle<PinView>,
    before: Option<&Bars>,
    meta: &Meta,
    cx: &mut AsyncApp,
) -> Result<Bars> {
    let count = pin
        .update(cx, |v, _, cx| {
            v.canvas
                .as_ref()
                .unwrap()
                .read(cx)
                .toolbar_config
                .tools
                .len()
        })
        .map_err(|e| e.to_string())?;
    let work = native::work_area((
        meta.placement.x + meta.placement.width as i32 / 2,
        meta.placement.y + meta.placement.height as i32 / 2,
    ));
    let expected = PinToolbarLayout::new_with_tool_count(meta.placement, work, meta.dpi, count);
    let mut last = None;
    for _ in 0..40 {
        frame(cx).await;
        if let Ok(actual) = bars(pin, cx) {
            let b = actual.secondary_rect;
            let secondary = PinToolbarLayout::properties(
                expected.primary,
                work,
                meta.dpi,
                (b.width as f32 / meta.dpi, b.height as f32 / meta.dpi),
                expected.properties_above,
            );
            if actual.primary_rect == expected.primary && actual.secondary_rect == secondary {
                if let Some(before) = before {
                    ensure(
                        actual.primary == before.primary && actual.secondary == before.secondary,
                        "纯缩放重建了两级工具栏 HWND",
                    )?;
                    ensure(
                        (actual.primary_rect.width, actual.primary_rect.height)
                            == (before.primary_rect.width, before.primary_rect.height)
                            && (actual.secondary_rect.width, actual.secondary_rect.height)
                                == (before.secondary_rect.width, before.secondary_rect.height),
                        "工具栏尺寸被图像缩放比例放大/缩小",
                    )?;
                }
                return Ok(actual);
            }
            last = Some(actual);
        }
    }
    Err(format!("缩放后工具栏没有靠图片布局/未收敛：{last:?}；预期 {expected:?}").into())
}
async fn hit_and_edit(
    pin: WindowHandle<PinView>,
    objects: &[Object],
    zoom: f32,
    cx: &mut AsyncApp,
) -> Result<serde_json::Value> {
    let input = input(pin, cx)?;
    let mut hit = Vec::new();
    for (tool_kind, point) in [
        (Tool::Rectangle, p(92., 76.)),
        (Tool::Arrow, p(256., 80.)),
        (Tool::Pen, p(96., 196.)),
        (Tool::Text, p(250., 177.)),
        (Tool::Spotlight, p(394., 76.)),
        (Tool::Mosaic, p(392., 200.)),
    ] {
        tool(pin, Tool::Edit, cx)?;
        let before = document(pin, cx)?;
        let physical = meta(pin, cx)?.viewport.window_point(point);
        input.click((physical.x / input.scale, physical.y / input.scale));
        for _ in 0..3 {
            frame(cx).await;
        }
        let selected = pin
            .update(cx, |v, _, cx| {
                v.canvas.as_ref().unwrap().read(cx).editor.selected
            })
            .map_err(|e| e.to_string())?;
        let expected = objects
            .iter()
            .find(|o| o.annotation.tool == tool_kind)
            .unwrap()
            .id;
        ensure(
            selected == Some(expected),
            &format!("{tool_kind:?} 在 {zoom} 倍原生命中错误：{selected:?} / {expected}"),
        )?;
        unchanged(&before, &document(pin, cx)?, "仅选中对象修改了文档/撤销栈")?;
        hit.push(
            serde_json::json!({"tool":format!("{tool_kind:?}"),"object":expected,
            "image_point":[point.x,point.y],"physical_point":[physical.x,physical.y]}),
        );
    }
    tool(pin, Tool::Edit, cx)?;
    let before = document(pin, cx)?;
    let viewport = meta(pin, cx)?.viewport;
    let a = viewport.window_point(p(92., 76.));
    let b = viewport.window_point(p(116., 92.));
    input.drag(
        (a.x.round() as u32, a.y.round() as u32),
        (b.x.round() as u32, b.y.round() as u32),
    );
    for _ in 0..4 {
        frame(cx).await;
    }
    let changed = document(pin, cx)?;
    let rectangle = objects
        .iter()
        .find(|o| o.annotation.tool == Tool::Rectangle)
        .unwrap();
    let mut moved = rectangle.annotation.clone();
    moved.translate(24., 16.);
    ensure(
        changed.objects.len() == objects.len()
            && changed.objects.iter().all(|o| {
                if o.id == rectangle.id {
                    let mut comparable = o.annotation.clone();
                    comparable.points = moved.points.clone();
                    comparable == moved
                        && o.annotation.points.len() == moved.points.len()
                        && o.annotation
                            .points
                            .iter()
                            .zip(&moved.points)
                            .all(|(a, b)| a.distance(*b) < 0.05)
                } else {
                    objects.iter().any(|original| original == o)
                }
            })
            && changed.edit_revision == before.edit_revision + 1,
        "缩放后的原生拖动没有只按原图单位移动矩形并提交一次",
    )?;
    tool(pin, Tool::Rectangle, cx)?;
    let moved_output = output(pin, cx)?;
    let native = framebuffer(pin, cx).await?;
    let edited_pixels = image_checks(&native, &moved_output, meta(pin, cx)?.viewport)?;
    export::save_png(
        native,
        &Path::new(FOLDER).join(format!("native-edit-{}pct.png", (zoom * 100.).round())),
    )?;
    action(pin, PinAction::Undo, cx)?;
    for _ in 0..3 {
        frame(cx).await;
    }
    ensure(
        document(pin, cx)?.objects == objects,
        "缩放后对象编辑的撤销没有恢复原文档",
    )?;
    tool(pin, Tool::Rectangle, cx)?;
    Ok(
        serde_json::json!({"hits":hit,"native_drag_image_delta":[24,16],"edited_frame_pixels":edited_pixels,
        "one_edit_commit":true,"undo_restores_document":true}),
    )
}
async fn wheel_anchor(
    pin: WindowHandle<PinView>,
    chrome: bool,
    baseline: &Image,
    logs: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let current = pin
        .update(cx, |v, _, _| v.chrome)
        .map_err(|e| e.to_string())?;
    if current != chrome {
        action(pin, PinAction::Chrome, cx)?;
    }
    action(pin, PinAction::Original, cx)?;
    for _ in 0..3 {
        frame(cx).await;
    }
    if chrome {
        tool(pin, Tool::Edit, cx)?;
    }
    let before = settle(pin, 1., cx).await?;
    let doc = document(pin, cx)?;
    // 普通滚轮以图片左上角为锚；消息仍从非中心指针位置发出。
    let anchor = p(0., 0.);
    let image_point = before.viewport.image_point(anchor);
    let global = p(
        before.placement.x as f32 + anchor.x,
        before.placement.y as f32 + anchor.y,
    );
    let input = input(pin, cx)?;
    input.wheel((180. / input.scale, 224. / input.scale), 120)?;
    let after = settle(pin, 1.1, cx).await?;
    let mapped = after.viewport.window_point(image_point);
    let now = p(
        after.placement.x as f32 + mapped.x,
        after.placement.y as f32 + mapped.y,
    );
    ensure(
        global.distance(now) <= 1.5,
        "普通滚轮缩放后图片左上角发生漂移",
    )?;
    unchanged(&doc, &document(pin, cx)?, "原生滚轮缩放改写了文档/撤销栈")?;
    same_image(&output(pin, cx)?, baseline, "原生滚轮缩放改变了原图导出")?;
    let native = framebuffer(pin, cx).await?;
    let checks = image_checks(&native, baseline, after.viewport)?;
    export::save_png(
        native,
        &Path::new(FOLDER).join(format!(
            "wheel-{}.png",
            if chrome { "editing" } else { "collapsed" }
        )),
    )?;
    logs.push(
        serde_json::json!({"phase":"native-wheel","editing":chrome,"anchor_mode":"image-top-left",
        "before":meta_log(&before),"after":meta_log(&after),
        "anchor_global":[global.x,global.y],"mapped_global":[now.x,now.y],
        "anchor_error_physical_px":global.distance(now),"pixels":checks}),
    );
    Ok(())
}
async fn pointer_anchor(
    pin: WindowHandle<PinView>,
    chrome: bool,
    baseline: &Image,
    logs: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let current = pin
        .update(cx, |v, _, _| v.chrome)
        .map_err(|e| e.to_string())?;
    if current != chrome {
        action(pin, PinAction::Chrome, cx)?;
    }
    action(pin, PinAction::Original, cx)?;
    let before = settle(pin, 1., cx).await?;
    let doc = document(pin, cx)?;
    let anchor = p(180., 224.);
    let global = p(
        before.placement.x as f32 + anchor.x,
        before.placement.y as f32 + anchor.y,
    );
    pin.update(cx, |v, w, cx| {
        v.zoom(1.5, Some((anchor.x, anchor.y)), w, cx)
    })
    .map_err(|e| e.to_string())??;
    let after = settle(pin, 1.5, cx).await?;
    let mapped = after
        .viewport
        .window_point(before.viewport.image_point(anchor));
    let actual = p(
        after.placement.x as f32 + mapped.x,
        after.placement.y as f32 + mapped.y,
    );
    ensure(
        global.distance(actual) <= 1.5,
        "显式指针锚点缩放后原图点发生漂移",
    )?;
    unchanged(&doc, &document(pin, cx)?, "显式锚点缩放改写了文档/撤销栈")?;
    let native = framebuffer(pin, cx).await?;
    let checks = image_checks(&native, baseline, after.viewport)?;
    export::save_png(
        native,
        &Path::new(FOLDER).join(format!(
            "pointer-{}.png",
            if chrome { "editing" } else { "collapsed" }
        )),
    )?;
    logs.push(
        serde_json::json!({"phase":"explicit-pointer-anchor","editing":chrome,
        "before":meta_log(&before),"after":meta_log(&after),
        "anchor_global":[global.x,global.y],"mapped_global":[actual.x,actual.y],
        "anchor_error_physical_px":global.distance(actual),"pixels":checks}),
    );

    // 中键在另一个非中心位置恢复 100%，检查真实原生消息所携带的物理位置。
    let anchor = p(230., 260.);
    let image_point = after.viewport.image_point(anchor);
    let global = p(
        after.placement.x as f32 + anchor.x,
        after.placement.y as f32 + anchor.y,
    );
    let input = input(pin, cx)?;
    let point = LPARAM((((anchor.y as u32) << 16) | anchor.x as u32) as isize);
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            WM_MBUTTONDOWN,
            WPARAM(0x10),
            point,
        )?;
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            WM_MBUTTONUP,
            WPARAM(0),
            point,
        )?;
    }
    let restored = settle(pin, 1., cx).await?;
    let mapped = restored.viewport.window_point(image_point);
    let actual = p(
        restored.placement.x as f32 + mapped.x,
        restored.placement.y as f32 + mapped.y,
    );
    ensure(
        global.distance(actual) <= 1.5,
        "中键恢复原尺寸没有保持指针下的原图点",
    )?;
    unchanged(&doc, &document(pin, cx)?, "中键恢复原尺寸改写了文档/撤销栈")?;
    same_image(
        &output(pin, cx)?,
        baseline,
        "指针锚点/中键缩放改变了导出像素",
    )?;
    let native = framebuffer(pin, cx).await?;
    let checks = image_checks(&native, baseline, restored.viewport)?;
    export::save_png(
        native,
        &Path::new(FOLDER).join(format!(
            "middle-{}.png",
            if chrome { "editing" } else { "collapsed" }
        )),
    )?;
    logs.push(
        serde_json::json!({"phase":"native-middle-original","editing":chrome,
        "before":meta_log(&after),"after":meta_log(&restored),
        "anchor_global":[global.x,global.y],"mapped_global":[actual.x,actual.y],
        "anchor_error_physical_px":global.distance(actual),"pixels":checks}),
    );
    Ok(())
}
async fn batched_zooms(
    pin: WindowHandle<PinView>,
    baseline: &Image,
    logs: &mut Vec<serde_json::Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    tool(pin, Tool::Rectangle, cx)?;
    action(pin, PinAction::Original, cx)?;
    let before = settle(pin, 1., cx).await?;
    let before_bars = toolbar_check(pin, None, &before, cx).await?;
    let doc = document(pin, cx)?;
    let anchor = p(180., 224.);
    let original_point = before.viewport.image_point(anchor);
    let global = p(
        before.placement.x as f32 + anchor.x,
        before.placement.y as f32 + anchor.y,
    );
    pin.update(cx, |v, w, cx| -> Result<()> {
        let actual = native::rect(hwnd(w))?;
        for zoom in [0.5, 2., 0.25, 1.5] {
            // 全部参数属于同一个真实客户区；不得改为读取各请求的 pending 尺寸。
            v.zoom(zoom, Some((anchor.x, anchor.y)), w, cx)?;
        }
        ensure(
            native::rect(hwnd(w))? == actual,
            "连续缩放在 GPUI 借用中同步重入定位",
        )
    })
    .map_err(|e| e.to_string())??;
    let after = settle(pin, 1.5, cx).await?;
    let mapped = after.viewport.window_point(original_point);
    let observed = p(
        after.placement.x as f32 + mapped.x,
        after.placement.y as f32 + mapped.y,
    );
    ensure(
        global.distance(observed) <= 1.5,
        "连续排队缩放把旧客户区锚点误当 pending 坐标",
    )?;
    unchanged(&doc, &document(pin, cx)?, "连续排队缩放改写文档/撤销栈")?;
    toolbar_check(pin, Some(&before_bars), &after, cx).await?;
    let native = framebuffer(pin, cx).await?;
    let checks = image_checks(&native, baseline, after.viewport)?;
    export::save_png(
        native,
        &Path::new(FOLDER).join("batched-pointer-150pct.png"),
    )?;
    logs.push(serde_json::json!({"phase":"batched-pointer-anchor","requested_scales":[0.5,2.,0.25,1.5],
        "before":meta_log(&before),"after":meta_log(&after),"one_actual_client_for_all_requests":true,
        "anchor_global":[global.x,global.y],"mapped_global":[observed.x,observed.y],
        "anchor_error_physical_px":global.distance(observed),"pixels":checks}));

    let before = after;
    let bars = toolbar_check(pin, None, &before, cx).await?;
    let center = p(
        before.placement.x as f32 + before.placement.width as f32 / 2.,
        before.placement.y as f32 + before.placement.height as f32 / 2.,
    );
    pin.update(cx, |v, w, cx| -> Result<()> {
        let actual = native::rect(hwnd(w))?;
        for zoom in [0.25, 1., 0.5, 2.] {
            v.zoom(zoom, None, w, cx)?;
        }
        ensure(
            native::rect(hwnd(w))? == actual,
            "默认中心缩放在 GPUI 借用中同步重入定位",
        )
    })
    .map_err(|e| e.to_string())??;
    let after = settle(pin, 2., cx).await?;
    let observed = p(
        after.placement.x as f32 + after.placement.width as f32 / 2.,
        after.placement.y as f32 + after.placement.height as f32 / 2.,
    );
    ensure(
        center.distance(observed) <= 1.5,
        "连续默认缩放没有保持图片中心/最后请求",
    )?;
    unchanged(&doc, &document(pin, cx)?, "连续默认缩放改写文档/撤销栈")?;
    same_image(&output(pin, cx)?, baseline, "连续缩放改变了导出像素")?;
    toolbar_check(pin, Some(&bars), &after, cx).await?;
    let native = framebuffer(pin, cx).await?;
    let checks = image_checks(&native, baseline, after.viewport)?;
    export::save_png(native, &Path::new(FOLDER).join("batched-center-200pct.png"))?;
    logs.push(
        serde_json::json!({"phase":"batched-center-latest-wins","requested_scales":[0.25,1.,0.5,2.],
        "before":meta_log(&before),"after":meta_log(&after),
        "center_error_physical_px":center.distance(observed),"pixels":checks}),
    );
    Ok(())
}
async fn settle_top_left(
    pin: WindowHandle<PinView>,
    zoom: f32,
    cx: &mut AsyncApp,
) -> Result<(Placement, Viewport)> {
    let expected = crate::pin_model::size((42, 945), zoom);
    for _ in 0..50 {
        frame(cx).await;
        let value = pin.update(cx, |v, w, cx| -> Result<_> {
            let actual = native::rect(hwnd(w))?;
            let canvas = v.canvas.as_ref().ok_or("缩放回归缺少画布")?.read(cx);
            Ok((actual, canvas.canvas_viewport(w), v.placement(w)?))
        })??;
        if (value.0.width, value.0.height) == expected && value.0 == value.2 {
            return Ok((value.0, value.1));
        }
    }
    Err("左上角缩放回归尺寸未收敛".into())
}

fn schedule_top_left(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    *errors.borrow_mut() = Some("左上角缩放回归未完成".into());
    crate::theme::set_mode(crate::theme::ThemeMode::System, cx);
    let color = crate::theme::palette(cx).background;
    let pixel = [color as u8, (color >> 8) as u8, (color >> 16) as u8, 255];
    let image = Image {
        width: 42,
        height: 945,
        bgra: pixel.repeat(42 * 945),
        readback_ms: 0.,
    };
    let initial = (|| -> Result<_> {
        ensure(
            crate::preferences::data_dir()
                == std::env::current_dir()?.join(FOLDER).join("settings")
                && std::env::var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY").as_deref() == Ok("1"),
            "缩放回归未隔离用户配置或托盘通知",
        )?;
        let pin = PinView::open_state(image, (100, 100), PinState::default(), cx)?;
        pin.update(cx, |v, w, cx| -> Result<()> {
            gpui_smoke::isolate_input(hwnd(w))?;
            v.ensure_canvas(w, cx)
        })??;
        Ok(pin)
    })();
    let pin = match initial {
        Ok(pin) => pin,
        Err(error) => {
            *errors.borrow_mut() = Some(error.to_string());
            cx.quit();
            return;
        }
    };
    cx.spawn(async move |cx| {
        let result: Result<()> = async {
            action(pin, PinAction::Original, cx)?;
            let (start, viewport) = settle_top_left(pin, 1., cx).await?;
            let baseline = output(pin, cx)?;
            let doc = document(pin, cx)?;
            let fixed = (
                f64::from(start.x) + f64::from(viewport.offset.x),
                f64::from(start.y) + f64::from(viewport.offset.y),
            );
            for _ in 0..3 {
                for zoom in [1.1, 1.2, 1.3, 1.4, 1.5, 1.4, 1.3, 1.2, 1.1, 1.] {
                    action(pin, PinAction::Zoom(zoom), cx)?;
                    let (actual, viewport) = settle_top_left(pin, zoom, cx).await?;
                    ensure(
                        (f64::from(actual.x) + f64::from(viewport.offset.x) - fixed.0).abs()
                            <= 0.501
                            && (f64::from(actual.y) + f64::from(viewport.offset.y) - fixed.1).abs()
                                <= 0.501,
                        "连续缩放的左上角累计偏移超过半个物理像素",
                    )?;
                }
                ensure(
                    settle_top_left(pin, 1., cx).await?.0 == start,
                    "缩回100%后窗口没有回到原位",
                )?;
            }
            pin.update(cx, |v, w, cx| {
                for zoom in [1.3, 0.7, 1.5, 1.] {
                    v.perform(PinAction::Zoom(zoom), w, cx);
                }
            })?;
            ensure(
                settle_top_left(pin, 1., cx).await?.0 == start,
                "同帧缩放合并后窗口没有回到原位",
            )?;
            unchanged(&doc, &document(pin, cx)?, "缩放回归修改文档/撤销栈")?;
            same_image(&output(pin, cx)?, &baseline, "缩放回归修改导出像素")?;
            println!("左上角缩放原生回归通过：30次缩放无累计漂移，文档与导出像素不变");
            Ok(())
        }
        .await;
        *errors.borrow_mut() = result.err().map(|error| error.to_string());
        let _ = pin.update(cx, |_, w, _| w.remove_window());
        let _ = cx.update(|cx| cx.quit());
    })
    .detach();
}

pub(crate) fn schedule(cx: &mut App, errors: Rc<RefCell<Option<String>>>) {
    if std::env::var("RUSTSHOT_PIN_ZOOM_DRIFT_ONLY").as_deref() == Ok("1") {
        schedule_top_left(cx, errors);
        return;
    }
    *errors.borrow_mut() = Some("贴图缩放专项未完成".into());
    let foreground = unsafe { GetForegroundWindow() }.0 as usize;
    let initial = (|| -> Result<_> {
        std::fs::create_dir_all(FOLDER)?;
        let settings = std::env::current_dir()?.join(FOLDER).join("settings");
        ensure(
            crate::preferences::data_dir() == settings,
            "贴图缩放专项未隔离用户配置",
        )?;
        std::fs::create_dir_all(&settings)?;
        std::fs::write(settings.join("tool-presets.json"), "{}\n")?;
        let work = native::work_area((100, 100));
        ensure(
            work.width >= WIDTH * 2 && work.height >= HEIGHT * 2,
            "25–200% 原生缩放专项需要至少 960×640 工作区",
        )?;
        let origin = (
            work.x + (work.width as i32 - WIDTH as i32) / 2,
            work.y + (work.height as i32 - HEIGHT as i32) / 2,
        );
        let pin = PinView::open_state(fixture(), origin, PinState::default(), cx)?;
        pin.update(cx, |v, w, cx| -> Result<_> {
            gpui_smoke::isolate_input(hwnd(w))?;
            v.ensure_canvas(w, cx)?;
            v.canvas.as_ref().unwrap().update(cx, |canvas, cx| {
                canvas.editor.document.insert_many(annotations());
                canvas.editor.select_tool(Tool::Rectangle);
                cx.notify();
            });
            v.perform(PinAction::Chrome, w, cx);
            Ok(())
        })??;
        Ok(pin)
    })();
    let pin = match initial {
        Ok(pin) => pin,
        Err(error) => {
            let text = error.to_string();
            let _ = std::fs::write(
                Path::new(FOLDER).join("report.json"),
                serde_json::json!({"ok":false,"error":text,"steps":[]}).to_string(),
            );
            *errors.borrow_mut() = Some(text);
            cx.quit();
            return;
        }
    };
    cx.spawn(async move |cx| {
        let mut logs = Vec::new();
        let result: Result<()> = async {
            let original_meta=settle(pin,1.,cx).await?;
            let baseline=output(pin,cx)?;
            let objects=document(pin,cx)?.objects;
            ensure(objects.len()==6,"缩放 QA 六类标注夹具不完整")?;
            export::save_png(copy_image(&baseline),&Path::new(FOLDER).join("document-export.png"))?;
            let first_bars=toolbar_check(pin,None,&original_meta,cx).await?;
            for zoom in [0.25_f32,0.5,1.,2.] {
                let before=document(pin,cx)?;
                let before_meta=meta(pin,cx)?;
                let before_bars=toolbar_check(pin,None,&before_meta,cx).await?;
                action(pin,PinAction::Zoom(zoom),cx)?;
                let actual=settle(pin,zoom,cx).await?;
                let before_top_left=p(before_meta.placement.x as f32+before_meta.viewport.offset.x,
                    before_meta.placement.y as f32+before_meta.viewport.offset.y);
                let actual_top_left=p(actual.placement.x as f32+actual.viewport.offset.x,
                    actual.placement.y as f32+actual.viewport.offset.y);
                ensure(before_top_left.distance(actual_top_left)<=1.5,"工具栏/菜单缩放没有保持图片左上角")?;
                logs.push(serde_json::json!({"phase":"zoom","zoom":zoom,"ok":false,
                    "native":meta_log(&actual),"checkpoint":"尺寸已收敛，检查真实帧"}));
                unchanged(&before,&document(pin,cx)?,"纯窗口缩放修改了文档/撤销栈")?;
                let native=framebuffer(pin,cx).await?;
                let path=Path::new(FOLDER).join(format!("native-{}pct.png",(zoom*100.).round()));
                export::save_png(copy_image(&native),&path)?;
                let checks=image_checks(&native,&baseline,actual.viewport)?;
                same_image(&output(pin,cx)?,&baseline,"缩放时导出尺寸/像素改变")?;
                unchanged(&before,&document(pin,cx)?,"纯导出修改了文档/撤销栈")?;
                let bar=toolbar_check(pin,Some(&before_bars),&actual,cx).await?;
                let edit=hit_and_edit(pin,&objects,zoom,cx).await?;
                same_image(&output(pin,cx)?,&baseline,"缩放后的编辑撤销改变了导出像素")?;
                // Edit 无选中会正常关闭二级栏；重新回到矩形后只检查布局与一级身份。
                let restored_bars=toolbar_check(pin,None,&actual,cx).await?;
                ensure(restored_bars.primary==first_bars.primary,"命中/编辑流程重建了一级工具栏")?;
                let index=logs.len()-1;
                logs[index]=serde_json::json!({"phase":"zoom","zoom":zoom,"ok":true,
                    "anchor_mode":"image-top-left","anchor_error_physical_px":before_top_left.distance(actual_top_left),
                    "native":meta_log(&actual),"pixels":checks,"interaction":edit,
                    "toolbar_handles":[bar.primary,bar.secondary],
                    "primary":[bar.primary_rect.x,bar.primary_rect.y,bar.primary_rect.width,bar.primary_rect.height],
                    "secondary":[bar.secondary_rect.x,bar.secondary_rect.y,bar.secondary_rect.width,bar.secondary_rect.height],
                    "document_unchanged_by_zoom_export":true,"export_pixels_exact":true});
            }
            wheel_anchor(pin,false,&baseline,&mut logs,cx).await?;
            wheel_anchor(pin,true,&baseline,&mut logs,cx).await?;
            pointer_anchor(pin,false,&baseline,&mut logs,cx).await?;
            pointer_anchor(pin,true,&baseline,&mut logs,cx).await?;
            batched_zooms(pin,&baseline,&mut logs,cx).await?;
            // 最后回到 100%，原图与全部标注仍然可编辑、可无损导出。
            tool(pin,Tool::Rectangle,cx)?;
            action(pin,PinAction::Original,cx)?;
            let final_meta=settle(pin,1.,cx).await?;
            ensure(document(pin,cx)?.objects==objects,"连续缩放结束后文档内容变化")?;
            same_image(&output(pin,cx)?,&baseline,"连续缩放结束后导出像素变化")?;
            let final_frame=framebuffer(pin,cx).await?;
            image_checks(&final_frame,&baseline,final_meta.viewport)?;
            export::save_png(final_frame,&Path::new(FOLDER).join("native-final-100pct.png"))?;
            Ok(())
        }.await;
        let error=result.err().map(|e|e.to_string());
        let report=serde_json::json!({"test":"native_pin_zoom_pixels_dpi_hit_edit_export_anchor",
            "ok":error.is_none(),"error":error,"steps":logs,
            "fixture_size":[WIDTH,HEIGHT],"scales":[0.25,0.5,1.,2.],
            "capture_source":"owned-gpui-framebuffer","clipboard_written":false});
        let saved=std::fs::write(Path::new(FOLDER).join("report.json"),
            serde_json::to_vec_pretty(&report).unwrap_or_default());
        println!("{report}");
        *errors.borrow_mut()=error.or_else(||saved.err().map(|e|e.to_string()));
        unsafe {
            let mut owner=0;
            GetWindowThreadProcessId(GetForegroundWindow(),Some(&mut owner));
            if owner==std::process::id() && foreground!=0 {
                let _=SetForegroundWindow(HWND(foreground as *mut _));
            }
        }
        let _=pin.update(cx,|v,w,cx|v.perform(PinAction::Close,w,cx));
        let _=cx.update(|cx|cx.quit());
    }).detach();
}
