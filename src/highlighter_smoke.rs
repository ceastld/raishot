//! 高亮专项：真实按钮/笔迹输入，仅采样和保存自有合成区域。
use crate::{
    annotation::{
        Annotation, HighlighterBlend as Blend, HighlighterShape as Shape, Point, Style, Tool,
    },
    canvas_source::CanvasSource,
    capture::Image,
    capture_view::CaptureView,
    editor::Editor,
    export,
    fast_capture::{FrozenDesktop, FrozenScreen},
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    toolbar_layout::{PanelRect, ToolbarPanel},
    Result,
};
use gpui::{AnyWindowHandle, App, AppContext, AsyncApp, Entity, RenderImage, WeakEntity, Window};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};
use windows::Win32::{
    Foundation::*,
    Graphics::{Dwm::DwmFlush, Gdi::ClientToScreen},
    System::Threading::GetCurrentProcessId,
    UI::WindowsAndMessaging::*,
};

const CASES: [&str; 4] = [
    "native_highlighter_line_source_multiply_overlap_alpha",
    "native_highlighter_selected_modes_shape_undo_redo",
    "native_highlighter_freehand_union_gpu_png_above_stable",
    "native_highlighter_zoomed_source_brush_tile_seam_bilinear",
];
fn pass(passed: &Rc<RefCell<Vec<&'static str>>>, index: usize) {
    passed.borrow_mut().push(CASES[index]);
    println!("{{\"test\":\"{}\",\"ok\":true}}", CASES[index]);
}
fn pixel(image: &Image, p: (u32, u32)) -> [u8; 4] {
    let index = ((p.1 * image.width + p.0) * 4) as usize;
    image.bgra[index..index + 4]
        .try_into()
        .expect("fixture pixel")
}
fn near(a: [u8; 4], b: [u8; 4], context: &str) -> Result<()> {
    if a.iter()
        .zip(b)
        .any(|(a, b)| (*a as i16 - b as i16).abs() > 2)
    {
        return Err(format!("{context}: BGRA {a:?} != {b:?}").into());
    }
    Ok(())
}
fn point(area: Rect, input: Input, p: (u32, u32)) -> (u32, u32) {
    (
        area.x + (p.0 as f32 * input.scale).round() as u32,
        area.y + (p.1 as f32 * input.scale).round() as u32,
    )
}
fn local(input: Input, p: (u32, u32)) -> (u32, u32) {
    (
        (p.0 as f32 * input.scale).round() as u32,
        (p.1 as f32 * input.scale).round() as u32,
    )
}
fn contains(r: PanelRect, p: (f32, f32)) -> bool {
    p.0 >= r.x && p.0 <= r.right() && p.1 >= r.y && p.1 <= r.bottom()
}
fn owned(input: Input, area: Rect) -> Result<()> {
    if std::env::var("RUSTSHOT_HIGHLIGHTER_QA_GPU_FRAMEBUFFER").as_deref() == Ok("1") {
        unsafe {
            let handle = HWND(input.handle as *mut _);
            let mut owner = 0;
            GetWindowThreadProcessId(handle, Some(&mut owner));
            if owner != GetCurrentProcessId() || !IsWindow(Some(handle)).as_bool() {
                return Err("自有 GPU 测试窗口已退出，停止输入与读回".into());
            }
        }
        return Ok(());
    }
    let background =
        std::env::var("RUSTSHOT_HIGHLIGHTER_QA_OWNED_BACKGROUND").as_deref() == Ok("1");
    unsafe {
        let h = HWND(input.handle as *mut _);
        let mut pid = 0;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid != GetCurrentProcessId()
            || !IsWindowVisible(h).as_bool()
            || (!background && GetForegroundWindow() != h)
        {
            let foreground = GetForegroundWindow();
            let mut foreground_pid = 0;
            GetWindowThreadProcessId(foreground, Some(&mut foreground_pid));
            return Err(format!(
                "自有高亮窗口未就绪/失去前台，停止输入与采样：own={:#x} pid={pid} isWindow={} visible={} foreground={:#x} pid={foreground_pid}",
                h.0 as usize, IsWindow(Some(h)).as_bool(), IsWindowVisible(h).as_bool(), foreground.0 as usize,
            ).into());
        }
        let check = |x, y| -> Result<()> {
            let mut p = POINT {
                x: x as i32,
                y: y as i32,
            };
            ClientToScreen(h, &mut p).ok()?;
            let covering = WindowFromPoint(p);
            if covering != h {
                return Err(format!(
                    "合成采样区域被其他窗口覆盖：own={:#x} point=({x},{y}) covering={:#x}",
                    h.0 as usize, covering.0 as usize
                )
                .into());
            }
            Ok(())
        };
        if background {
            // WindowFromPoint 忽略禁用窗口；另外拒绝 Z 序上方任何相交的可见窗口。
            let mut origin = POINT {
                x: area.x as i32,
                y: area.y as i32,
            };
            ClientToScreen(h, &mut origin).ok()?;
            let mut previous = GetWindow(h, GW_HWNDPREV).ok();
            let mut visited = std::collections::HashSet::new();
            while let Some(covering) = previous {
                if !visited.insert(covering.0 as usize) {
                    return Err("窗口 Z 序在检查时变化，停止采样".into());
                }
                if IsWindowVisible(covering).as_bool() {
                    let mut bounds = RECT::default();
                    GetWindowRect(covering, &mut bounds)?;
                    if bounds.left < origin.x + area.width as i32
                        && bounds.right > origin.x
                        && bounds.top < origin.y + area.height as i32
                        && bounds.bottom > origin.y
                    {
                        return Err(format!(
                            "合成区域上方存在相交可见窗口，停止输入与采样：own={:#x} covering={:#x}",
                            h.0 as usize, covering.0 as usize
                        ).into());
                    }
                }
                previous = GetWindow(covering, GW_HWNDPREV).ok();
            }
            // 只验后台可见渲染，不假定前台；逐像素保护整个取样区域，禁止读到其它窗口。
            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    check(x, y)?;
                }
            }
        } else {
            for y in [area.y, area.y + area.height / 2, area.y + area.height - 1] {
                for x in [area.x, area.x + area.width / 2, area.x + area.width - 1] {
                    check(x, y)?;
                }
            }
        }
    }
    Ok(())
}
fn install(v: &mut CaptureView, input: Input) -> Result<Rect> {
    let monitor = v.source.screen_rect(v.source.current);
    if monitor.width as f32 / input.scale < 1000. || monitor.height as f32 / input.scale < 720. {
        return Err("高亮原生专项需要至少1000×720逻辑像素显示器".into());
    }
    crate::spotlight_smoke::install(v)?;
    let area = Rect {
        x: monitor.x + (70. * input.scale) as u32,
        y: monitor.y + (280. * input.scale) as u32,
        width: (820. * input.scale) as u32,
        height: (360. * input.scale) as u32,
    };
    let bounds = v.source.bounds;
    let mut bytes = [160, 160, 160, 255].repeat(bounds.width as usize * bounds.height as usize);
    for y in local(input, (0, 40)).1..local(input, (0, 220)).1 {
        for (range, color) in [(200..260, [0, 0, 0, 255]), (340..460, [210, 90, 60, 255])] {
            for x in local(input, (range.start, 0)).0..local(input, (range.end, 0)).0 {
                let index = (((area.y + y) * bounds.width + area.x + x) * 4) as usize;
                bytes[index..index + 4].copy_from_slice(&color);
            }
        }
    }
    let preview = Arc::new(RenderImage::new(vec![image::Frame::new(
        image::RgbaImage::from_raw(bounds.width, bounds.height, bytes).ok_or("合成图尺寸错误")?,
    )]));
    let (source, previews) = CanvasSource::frozen_desktop(FrozenDesktop {
        bounds,
        screens: vec![FrozenScreen {
            rect: bounds,
            preview,
        }],
        current: 0,
        capture_ms: 0.,
    });
    v.source = source;
    v.previews = previews;
    v.editor = Editor::new(v.source.full(), Some(area));
    v.editor.select_tool(Tool::Highlighter);
    v.toolbar_config.tools = vec![
        Tool::Highlighter,
        Tool::HighlighterRect,
        Tool::Pen,
        Tool::Rectangle,
    ];
    v.toolbar_position = Some((
        area.x as f32 / input.scale,
        area.y as f32 / input.scale - 60.,
    ));
    v.toolbar_panel = ToolbarPanel::None;
    v.ui_positions.borrow_mut().clear();
    Ok(area)
}
async fn click(
    v: &WeakEntity<CaptureView>,
    input: Input,
    id: &'static str,
    cx: &mut AsyncApp,
) -> Result<()> {
    for _ in 0..20 {
        if let Some(p) = v
            .update(cx, |v, _| v.ui_positions.borrow().get(id).copied())
            .map_err(|e| e.to_string())?
        {
            owned(
                input,
                Rect {
                    x: (p.0 * input.scale).round() as u32,
                    y: (p.1 * input.scale).round() as u32,
                    width: 1,
                    height: 1,
                },
            )?;
            input.click(p);
            crate::spotlight_smoke::frame(cx).await;
            return Ok(());
        }
        crate::spotlight_smoke::frame(cx).await;
    }
    Err(format!("高亮按钮未绘制：{id}").into())
}
async fn stroke(
    v: &WeakEntity<CaptureView>,
    input: Input,
    area: Rect,
    points: &[(u32, u32)],
    cx: &mut AsyncApp,
) -> Result<()> {
    owned(input, area)?;
    // 明确重新选绘制工具，清掉上一笔选中手柄；起点也必须在旧笔迹之外。
    click(v, input, "highlighter", cx).await?;
    input.stroke(
        &points
            .iter()
            .map(|p| point(area, input, *p))
            .collect::<Vec<_>>(),
    )?;
    crate::spotlight_smoke::frame(cx).await;
    println!(
        "{{\"phase\":\"highlighter_qa\",\"checkpoint\":\"stroke\",\"input_points\":{}}}",
        points.len()
    );
    Ok(())
}
async fn output(v: &WeakEntity<CaptureView>, cx: &mut AsyncApp) -> Result<Image> {
    v.update(cx, |v, _| v.selected_image())
        .map_err(|e| e.to_string())?
}
async fn capture_pixels(
    input: Input,
    gpu_window: AnyWindowHandle,
    area: Rect,
    cx: &mut AsyncApp,
) -> Result<Image> {
    owned(input, area)?;
    if std::env::var("RUSTSHOT_HIGHLIGHTER_QA_GPU_FRAMEBUFFER").as_deref() == Ok("1") {
        cx.update_window(gpu_window, |_, window, _| {
            window.request_frame_capture([area.x, area.y, area.width, area.height])
        })
        .map_err(|e| e.to_string())??;
        for _ in 0..20 {
            crate::spotlight_smoke::frame(cx).await;
            if let Some(result) = cx
                .update_window(gpu_window, |_, window, _| window.take_frame_capture())
                .map_err(|e| e.to_string())?
            {
                let bgra = result.map_err(|e| e.to_string())?;
                owned(input, area)?;
                return Ok(Image {
                    width: area.width,
                    height: area.height,
                    bgra,
                    readback_ms: 0.,
                });
            }
        }
        return Err("自有 GPU 帧缓冲读回超时".into());
    }
    unsafe {
        DwmFlush()?;
    }
    let image = crate::spotlight_smoke::native_owned_region(area)?;
    owned(input, area)?;
    Ok(image)
}
#[allow(clippy::too_many_arguments)]
async fn native_png(
    v: &WeakEntity<CaptureView>,
    input: Input,
    gpu_window: AnyWindowHandle,
    area: Rect,
    folder: &Path,
    name: &str,
    samples: &[(u32, u32)],
    cx: &mut AsyncApp,
) -> Result<Image> {
    // 悬停笔径预览也属于编辑辅助层，移到取样框外，避免误判为笔迹透明度。
    input.hover(point(area, input, (780, 345)))?;
    crate::spotlight_smoke::frame(cx).await;
    let native = capture_pixels(input, gpu_window, area, cx).await?;
    println!(
        "{{\"phase\":\"highlighter_qa\",\"checkpoint\":\"nativeSnippet\",\"name\":\"{name}\"}}"
    );
    let output = output(v, cx).await?;
    for p in samples {
        near(
            pixel(&native, local(input, *p)),
            pixel(&output, local(input, *p)),
            "GPU/导出内点",
        )?;
    }
    let path = folder.join(format!("{name}-output.png"));
    export::save_png(output, &path)?;
    let decoded = image::open(path)?.into_rgba8();
    for p in samples {
        let p = local(input, *p);
        let rgba = decoded.get_pixel(p.0, p.1).0;
        near(
            pixel(&native, p),
            [rgba[2], rgba[1], rgba[0], rgba[3]],
            "GPU/PNG内点",
        )?;
    }
    Ok(native)
}
fn alpha(source: [u8; 4]) -> [u8; 4] {
    let tint = [59, 212, 255];
    let mut out = source;
    for (c, tint) in tint.into_iter().enumerate() {
        out[c] = ((source[c] as u32 * 159 + tint * 96 + 127) / 255) as u8;
    }
    out
}
fn seam_texel(x: u32, y: u32) -> [u8; 4] {
    [
        40 + (x % 37) as u8 * 4,
        60 + (y % 41) as u8 * 3,
        if (x / 5 + y / 5).is_multiple_of(2) {
            220
        } else {
            80
        },
        255,
    ]
}
fn seam_golden(x: f32, y: f32) -> [u8; 4] {
    // D3D线性采样以纹素中心为整数；同一个原始颜色函数独立计算底图和乘色画刷。
    let x = x - 0.5;
    let y = y - 0.5;
    let base = (x.floor() as u32, y.floor() as u32);
    let fx = x.fract();
    let fy = y.fract();
    let mut result = [0; 4];
    for (c, tint) in [255, 215, 44].into_iter().enumerate() {
        let mut original = 0.;
        let mut brush = 0.;
        for (dy, wy) in [(0, 1. - fy), (1, fy)] {
            for (dx, wx) in [(0, 1. - fx), (1, fx)] {
                let value = seam_texel(base.0 + dx, base.1 + dy)[c] as u32;
                original += value as f32 * wx * wy;
                brush += (value * tint / 255) as f32 * wx * wy;
            }
        }
        result[c] = (original * 0.45 + brush * 0.55).round() as u8;
    }
    result[3] = 255;
    result
}
async fn seam(
    v: &WeakEntity<CaptureView>,
    input: Input,
    gpu_window: AnyWindowHandle,
    area: Rect,
    folder: &Path,
    cx: &mut AsyncApp,
) -> Result<()> {
    let offset = Point {
        x: (area.x - (70. * input.scale) as u32) as f32 + 50.,
        y: (area.y - (280. * input.scale) as u32) as f32 + 350.,
    };
    let zoom = 0.37;
    let mut client = RECT::default();
    unsafe {
        GetClientRect(HWND(input.handle as *mut _), &mut client)?;
    }
    v.update(cx, |v, cx| -> Result<()> {
        let image = Image {
            width: 4096,
            height: 256,
            bgra: (0..256)
                .flat_map(|y| (0..4096).flat_map(move |x| seam_texel(x, y)))
                .collect(),
            readback_ms: 0.,
        };
        let (source, previews) = CanvasSource::image(image)?;
        v.source = source;
        v.previews = previews;
        v.editor = Editor::new(v.source.full(), Some(v.source.full()));
        let mut mark = Annotation::new(
            Tool::HighlighterRect,
            Point { x: 1200., y: 20. },
            Style {
                color: 0x2cd7ff,
                opacity: 0.55,
                highlighter_blend: Blend::Multiply,
                ..crate::tool_presets::default_style(Tool::HighlighterRect)
            },
        );
        mark.update(Point { x: 3000., y: 236. });
        v.editor.document.insert(mark);
        v.editor.select_tool(Tool::HighlighterRect);
        v.image_canvas = Some(crate::image_editor::ImageCanvas {
            path: None,
            viewport: crate::viewport::Viewport { zoom, offset },
            auto_fit: false,
            host: (
                (client.right - client.left) as u32,
                (client.bottom - client.top) as u32,
            ),
            pan: None,
        });
        v.toolbar_position = Some((16., 16.));
        v.ui_positions.borrow_mut().clear();
        v.finish_preview = None;
        let scene = v.prepared_scene();
        let Some(crate::scene::Layer::Highlighter { tiles, .. }) = scene.layers.first() else {
            return Err("跨tile高亮未使用矢量源图画刷".into());
        };
        if !scene.errors.is_empty()
            || tiles.len() != 2
            || !tiles.iter().any(|t| t.rect.x == 2048)
            || tiles
                .iter()
                .any(|t| t.image.bgra.chunks_exact(4).any(|p| p[3] != 255))
        {
            return Err("跨tile画刷分块/透明度合同不正确".into());
        }
        cx.notify();
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    crate::spotlight_smoke::frame(cx).await;
    crate::spotlight_smoke::frame(cx).await;
    let region = Rect {
        x: (offset.x + 2048. * zoom).floor() as u32 - 12,
        y: (offset.y + 128. * zoom).floor() as u32 - 10,
        width: 25,
        height: 21,
    };
    let native = capture_pixels(input, gpu_window, region, cx).await?;
    println!(
        "{{\"phase\":\"highlighter_qa\",\"checkpoint\":\"nativeSnippet\",\"name\":\"tile-seam\"}}"
    );
    let mut maximum = 0i16;
    for y in [2, 10, 18] {
        for x in 0..region.width {
            let golden = seam_golden(
                (region.x as f32 + x as f32 + 0.5 - offset.x) / zoom,
                (region.y as f32 + y as f32 + 0.5 - offset.y) / zoom,
            );
            let actual = pixel(&native, (x, y));
            for (a, b) in actual.iter().zip(golden) {
                maximum = maximum.max((*a as i16 - b as i16).abs());
            }
            if actual
                .iter()
                .zip(golden)
                .any(|(a, b)| (*a as i16 - b as i16).abs() > 3)
            {
                return Err(format!(
                    "2048分界缩小采样偏差：x{x}/y{y}, BGRA{actual:?}, golden{golden:?}"
                )
                .into());
            }
        }
    }
    let output = output(v, cx).await?;
    for x in 2045..2052 {
        let source = seam_texel(x, 128);
        let mut expected = source;
        for (c, tint) in [255, 215, 44].into_iter().enumerate() {
            expected[c] = (source[c] as f32 * 0.45 + (source[c] as u32 * tint / 255) as f32 * 0.55)
                .round() as u8;
        }
        near(pixel(&output, (x, 128)), expected, "PNG原始分界内点")?;
    }
    export::save_png(output, &folder.join("tile-seam-output.png"))?;
    export::save_png(native, &folder.join("tile-seam-native-owned.png"))?;
    println!("{{\"phase\":\"highlighter_seam\",\"max_channel_error\":{maximum},\"zoom\":{zoom}}}");
    Ok(())
}
async fn verify(
    v: WeakEntity<CaptureView>,
    input: Input,
    gpu_window: AnyWindowHandle,
    folder: &Path,
    passed: &Rc<RefCell<Vec<&'static str>>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    std::fs::create_dir_all(folder)?;
    let area = v
        .update(cx, |v, cx| {
            let result = install(v, input);
            cx.notify();
            result
        })
        .map_err(|e| e.to_string())??;
    let mut initial_error = None;
    let attempts = if std::env::var_os("RUSTSHOT_HIGHLIGHTER_QA_WAIT_ACTIVATION").is_some() {
        333
    } else {
        16
    };
    for attempt in 1..=attempts {
        if attempt == 1
            && std::env::var("RUSTSHOT_HIGHLIGHTER_QA_OWNED_BACKGROUND").as_deref() == Ok("1")
        {
            // 后台检查只将自己的已建窗口显示在上层，不激活或改动其它窗口。
            unsafe {
                let handle = HWND(input.handle as *mut _);
                let mut owner = 0;
                GetWindowThreadProcessId(handle, Some(&mut owner));
                if owner == GetCurrentProcessId() && IsWindowVisible(handle).as_bool() {
                    SetWindowPos(
                        handle,
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
                    )?;
                }
            }
        }
        if attempt == 1
            && std::env::var("RUSTSHOT_HIGHLIGHTER_QA_GPU_FRAMEBUFFER").as_deref() != Ok("1")
            && std::env::var("RUSTSHOT_HIGHLIGHTER_QA_OWNED_BACKGROUND").as_deref() != Ok("1")
        {
            // 工厂中的前台请求可能早于原生窗口就绪，仅给本进程窗口补一次普通激活。
            unsafe {
                let handle = HWND(input.handle as *mut _);
                let mut owner = 0;
                GetWindowThreadProcessId(handle, Some(&mut owner));
                if owner == GetCurrentProcessId() && IsWindowVisible(handle).as_bool() {
                    let _ = SetForegroundWindow(handle);
                }
            }
        }
        crate::spotlight_smoke::frame(cx).await;
        match owned(input, area) {
            Ok(()) => {
                initial_error = None;
                println!("{{\"phase\":\"highlighter_qa\",\"checkpoint\":\"initialOwned\",\"attempt\":{attempt}}}");
                break;
            }
            Err(error) => initial_error = Some(error.to_string()),
        }
    }
    if let Some(error) = initial_error {
        return Err(
            format!("首次自有窗口{attempts}帧等待后仍未就绪/前台受外部限制：{error}").into(),
        );
    }
    // 等待激活时用户的点击只用于交给前台，重新安装夹具以排除该点击的标注。
    v.update(cx, |v, cx| {
        let result = install(v, input);
        cx.notify();
        result
    })
    .map_err(|e| e.to_string())??;
    crate::spotlight_smoke::frame(cx).await;
    owned(input, area)?;
    let primary = v
        .update(cx, |v, _| v.toolbar_layout(area, input.scale).primary)
        .map_err(|e| e.to_string())?;
    let mut native_rect = RECT::default();
    unsafe {
        GetWindowRect(HWND(input.handle as *mut _), &mut native_rect)?;
    }
    if std::env::var("RUSTSHOT_HIGHLIGHTER_QA_PHASE").as_deref() != Ok("remaining") {
        click(&v, input, "highlighter-blend-alpha", cx).await?;
        click(&v, input, "highlighter-blend-multiply", cx).await?;
        click(&v, input, "highlighter-shape-line", cx).await?;
        stroke(
            &v,
            input,
            area,
            &[(80, 80), (250, 130), (440, 40), (700, 80)],
            cx,
        )
        .await?;
        let first = v
            .update(cx, |v, _| v.editor.document.items().to_vec())
            .map_err(|e| e.to_string())?;
        if first.len() != 1
            || first[0].annotation.points.len() != 2
            || first[0].annotation.style.highlighter_shape != Shape::Line
            || first[0].annotation.style.highlighter_blend != Blend::Multiply
        {
            return Err("多拐点输入未生成两端点叠底拉线".into());
        }
        let source = v
            .update(cx, |v, _| v.source.read_region(area))
            .map_err(|e| e.to_string())??;
        let once = output(&v, cx).await?;
        for p in [(120, 80), (230, 80), (380, 80)] {
            let mut expected = pixel(&source, local(input, p));
            for (c, tint) in [59, 212, 255].into_iter().enumerate() {
                expected[c] = (expected[c] as u32 * tint / 255) as u8;
            }
            near(
                pixel(&once, local(input, p)),
                expected,
                "原始灰/黑/彩底乘色",
            )?;
        }
        native_png(
            &v,
            input,
            gpu_window,
            area,
            folder,
            "multiply",
            &[(120, 84), (230, 84), (380, 84)],
            cx,
        )
        .await?;
        stroke(&v, input, area, &[(40, 80), (400, 160), (740, 80)], cx).await?;
        let twice = output(&v, cx).await?;
        for p in [(120, 80), (230, 80), (380, 80)] {
            near(
                pixel(&twice, local(input, p)),
                pixel(&once, local(input, p)),
                "两笔叠底重叠不加深",
            )?;
        }
        if v.update(cx, |v, _| v.editor.document.items().len())
            .map_err(|e| e.to_string())?
            != 2
        {
            return Err("第二次输入没有新建重叠笔迹".into());
        }
        native_png(
            &v,
            input,
            gpu_window,
            area,
            folder,
            "multiply-overlap",
            &[(120, 84), (230, 84), (380, 84)],
            cx,
        )
        .await?;
        click(&v, input, "highlighter", cx).await?;
        click(&v, input, "highlighter-blend-alpha", cx).await?;
        stroke(&v, input, area, &[(80, 180), (350, 210), (700, 180)], cx).await?;
        let overlay = output(&v, cx).await?;
        for p in [(120, 180), (230, 180), (380, 180)] {
            near(
                pixel(&overlay, local(input, p)),
                alpha(pixel(&source, local(input, p))),
                "透明叠加颜色",
            )?;
        }
        if pixel(&overlay, local(input, (120, 180))) == pixel(&once, local(input, (120, 80))) {
            return Err("Alpha与Multiply没有不同颜色结果".into());
        }
        native_png(
            &v,
            input,
            gpu_window,
            area,
            folder,
            "alpha",
            &[(120, 184), (230, 184), (380, 184)],
            cx,
        )
        .await?;
        pass(passed, 0);
        let before = v
            .update(cx, |v, _| {
                (
                    v.editor.document.items().to_vec(),
                    v.editor.document.edit_revision(),
                    v.editor.document.revision(),
                )
            })
            .map_err(|e| e.to_string())?;
        click(&v, input, "highlighter-shape-freehand", cx).await?;
        v.update(cx, |v, _| -> Result<()> {
            if v.editor.document.items() != before.0
                || v.editor.document.edit_revision() != before.1
                || v.editor.document.revision() != before.2
                || v.editor.highlighter_shape() != Shape::Freehand
            {
                return Err("切下一笔手绘修改了选中拉线/历史".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;
        click(&v, input, "highlighter-blend-multiply", cx).await?;
        v.update(cx, |v, _| -> Result<()> {
            let mut expected = before.0.clone();
            let last = expected.last_mut().ok_or("缺少选中对象")?;
            last.annotation.style.highlighter_blend = Blend::Multiply;
            last.annotation.style.opacity = 1.;
            if v.editor.document.items() != expected
                || v.editor.document.edit_revision() != before.1 + 1
            {
                return Err("选中对象混合方式不是一次事务/改变其他对象".into());
            }
            Ok(())
        })
        .map_err(|e| e.to_string())??;
        click(&v, input, "undo", cx).await?;
        if v.update(cx, |v, _| v.editor.document.items().to_vec())
            .map_err(|e| e.to_string())?
            != before.0
        {
            return Err("高亮切模式一次撤销未还原".into());
        }
        click(&v, input, "redo", cx).await?;
        if v.update(cx, |v, _| {
            v.editor
                .document
                .items()
                .last()
                .unwrap()
                .annotation
                .style
                .highlighter_blend
        })
        .map_err(|e| e.to_string())?
            != Blend::Multiply
        {
            return Err("高亮模式重做未恢复".into());
        }
        pass(passed, 1);
    }
    click(&v, input, "highlighter", cx).await?;
    click(&v, input, "highlighter-blend-alpha", cx).await?;
    click(&v, input, "highlighter-shape-freehand", cx).await?;
    stroke(
        &v,
        input,
        area,
        &[
            (90, 280),
            (310, 280),
            (310, 320),
            (180, 240),
            (180, 340),
            (260, 280),
            (90, 280),
        ],
        cx,
    )
    .await?;
    v.update(cx, |v, _| -> Result<()> {
        let last = &v
            .editor
            .document
            .items()
            .last()
            .ok_or("手绘未提交")?
            .annotation;
        if last.points.len() <= 2 || last.style.highlighter_shape != Shape::Freehand {
            return Err("自交手绘被当作拉线".into());
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    click(&v, input, "highlighter", cx).await?;
    let native = native_png(
        &v,
        input,
        gpu_window,
        area,
        folder,
        "freehand",
        &[(180, 280), (140, 280), (180, 320)],
        cx,
    )
    .await?;
    let finished = output(&v, cx).await?;
    let full = alpha([160, 160, 160, 255]);
    export::save_png(
        Image {
            width: native.width,
            height: native.height,
            bgra: native.bgra.clone(),
            readback_ms: native.readback_ms,
        },
        &folder.join("freehand-native.png"),
    )?;
    for p in [(180, 280), (140, 280), (180, 320)] {
        near(pixel(&finished, local(input, p)), full, "单笔自交覆盖并集")?;
    }
    let mut partial = 0;
    for y in local(input, (0, 235)).1..local(input, (0, 350)).1 {
        for x in local(input, (75, 0)).0..local(input, (325, 0)).0 {
            let color = pixel(&native, (x, y));
            if color[0] < full[0].saturating_sub(2)
                || color[1] > full[1] + 2
                || color[2] > full[2] + 2
            {
                return Err(format!(
                    "手绘覆盖颜色越界 x={x}, y={y}: BGRA={color:?}, full={full:?}"
                )
                .into());
            }
            if color[0] > full[0] + 2 && color[0] < 158 {
                partial += 1;
            }
        }
    }
    if partial < 8 {
        return Err("原生手绘边缘缺少连续矢量部分覆盖像素".into());
    }
    v.update(cx, |v, _| -> Result<()> {
        let layout = v.toolbar_layout(area, input.scale);
        let positions = v.ui_positions.borrow();
        if layout.primary != primary || !layout.properties_above {
            return Err("绘制/切模式改变一级位置或上方属性方向".into());
        }
        for id in [
            "highlighter-blend-multiply",
            "highlighter-blend-alpha",
            "highlighter-shape-freehand",
            "highlighter-shape-line",
        ] {
            if !positions
                .get(id)
                .is_some_and(|p| contains(layout.properties, *p))
            {
                return Err(format!("新增高亮按钮超出上方二级栏：{id}").into());
            }
        }
        Ok(())
    })
    .map_err(|e| e.to_string())??;
    let mut final_rect = RECT::default();
    unsafe {
        GetWindowRect(HWND(input.handle as *mut _), &mut final_rect)?;
    }
    if final_rect != native_rect {
        return Err("高亮输入改变宿主原生窗口尺寸".into());
    }
    export::save_png(native, &folder.join("freehand-native.png"))?;
    pass(passed, 2);
    seam(&v, input, gpu_window, area, folder, cx).await?;
    pass(passed, 3);
    Ok(())
}
pub(crate) fn schedule(
    view: &Entity<CaptureView>,
    window: &mut Window,
    _: &mut App,
    errors: Rc<RefCell<Option<String>>>,
) {
    *errors.borrow_mut() = Some("高亮专项尚未完成".into());
    let weak = view.downgrade();
    let input = Input {
        handle: hwnd(window).0 as usize,
        scale: window.scale_factor(),
    };
    let folder = std::env::var_os("RUSTSHOT_HIGHLIGHTER_QA_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/qa/highlighter"));
    window.on_next_frame(move|window,cx| { let gpu_window=window.window_handle(); cx.spawn(async move|cx| {
        let passed=Rc::new(RefCell::new(Vec::new()));
        let result=verify(weak,input,gpu_window,&folder,&passed,cx).await;
        let mut error=result.err().map(|e|e.to_string());
        let background=std::env::var("RUSTSHOT_HIGHLIGHTER_QA_OWNED_BACKGROUND").as_deref()==Ok("1");
        let gpu=std::env::var("RUSTSHOT_HIGHLIGHTER_QA_GPU_FRAMEBUFFER").as_deref()==Ok("1");
        let report=serde_json::json!({"ok":error.is_none(),"error":error,"scale":input.scale,"cases":*passed.borrow(),"pixel_backend":if gpu {"gpu_framebuffer"} else {"desktop_composition"},"foreground_required":!(background||gpu),"roi_ownership":if gpu {"owned_gpu_texture_crop"} else if background {"every_physical_pixel_before_and_after"} else {"nine_points_before_and_after"},"reject_visible_windows_above_roi":background&&!gpu});
        if let Err(e)=std::fs::create_dir_all(&folder).and_then(|()|std::fs::write(folder.join("summary.json"),report.to_string())) { error=Some(format!("报告写入失败：{e}，原始错误{error:?}")); }
        *errors.borrow_mut()=error; let _=cx.update(|cx|cx.quit());
        unsafe { let _=PostMessageW(Some(HWND(input.handle as *mut _)),WM_NULL,WPARAM(0),LPARAM(0)); }
    }).detach(); });
}
