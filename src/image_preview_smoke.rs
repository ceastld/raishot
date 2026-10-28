//! 长图文件编辑器：生产解码/窗口入口、真实自有输入、有界投影与正常关闭。
use crate::{
    annotation::{Point, Tool},
    capture::Image,
    capture_view::CaptureView,
    geometry::Rect,
    gpui_capture::hwnd,
    gpui_smoke::{Input, SMOKE_MOVE},
    image_editor::ImageInputProbe,
    pin_model::PinState,
    pin_view::{PinAction, PinView},
    process_memory::{cpu_memory, gpu_memory},
    scene::{Layer, SourceIdentity},
    stitch::{ScrollAxis, StitchSegment, StitchSnapshot},
    text_contract_smoke::{self, AdapterStats},
    Result,
};
use gpui::{
    div, prelude::*, App, AppContext, Application, AsyncApp, Render, RenderImage, Window,
    WindowHandle, WindowOptions,
};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, WPARAM},
    Graphics::Gdi::ClientToScreen,
    UI::{
        Input::KeyboardAndMouse::VIRTUAL_KEY,
        WindowsAndMessaging::{
            IsWindow, PostMessageW, WM_CLOSE, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEMOVE,
        },
    },
};

const SHORT: u32 = 960;
const LONG: u32 = 32_000;
const WAIT: Duration = Duration::from_secs(20);
const CLOSE_LIMIT: u64 = 16 * 1024 * 1024;
const RENDER_ALLOWANCE: u64 = 96 * 1024 * 1024;
type ImageWindow = WindowHandle<CaptureView>;
#[path = "image_preview_pin_smoke.rs"]
mod pin_entry;

fn ensure(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
async fn tick(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(25))
        .await;
}
fn unix_ms() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}
#[derive(Clone, Copy)]
struct Fixture {
    alpha: bool,
    horizontal: bool,
}
impl Fixture {
    fn dimensions(self) -> (u32, u32) {
        if self.horizontal {
            (LONG, SHORT)
        } else {
            (SHORT, LONG)
        }
    }
    fn xy(self, short: u32, long: u32) -> (u32, u32) {
        if self.horizontal {
            (long, short)
        } else {
            (short, long)
        }
    }
    fn pixel(self, x: u32, y: u32) -> [u8; 4] {
        let (short, long) = if self.horizontal { (y, x) } else { (x, y) };
        let a = short / 480;
        let b = long / 1024;
        [
            (32 + (a * 47 + b * 13) % 192) as u8,
            (32 + (a * 31 + b * 17) % 192) as u8,
            (32 + (a * 61 + b * 29) % 192) as u8,
            if self.alpha && short >= 880 {
                [0, 64, 128, 192, 255][((short + long) % 5) as usize]
            } else {
                255
            },
        ]
    }
    fn matte(self, x: u32, y: u32) -> [u8; 4] {
        let pixel = self.pixel(x, y);
        let a = u32::from(pixel[3]);
        let checker = if (x / 16 + y / 16).is_multiple_of(2) {
            224
        } else {
            192
        };
        [0, 1, 2, 3].map(|channel| {
            if channel == 3 {
                255
            } else {
                ((u32::from(pixel[channel]) * a + checker * (255 - a) + 127) / 255) as u8
            }
        })
    }
    fn verify(self, rect: Rect, bytes: &[u8]) -> Result<()> {
        ensure(
            bytes.len() == rect.width as usize * rect.height as usize * 4,
            "文件独立 oracle 字节长度错误",
        )?;
        for (y, row) in bytes.chunks_exact(rect.width as usize * 4).enumerate() {
            for (x, actual) in row.chunks_exact(4).enumerate() {
                let expected = self.pixel(rect.x + x as u32, rect.y + y as u32);
                if actual != expected {
                    return Err(format!("文件原图/alpha 独立像素错误 ({},{}) actual={actual:?} expected={expected:?}", rect.x + x as u32, rect.y + y as u32).into());
                }
            }
        }
        Ok(())
    }
}

// 文件夹具只生成一行 RGBA；创建 PNG 不预热另一个 117MiB 原图缓冲。
fn write_fixture(path: &Path, fixture: Fixture) -> Result<()> {
    let (width, height) = fixture.dimensions();
    let mut encoder = png::Encoder::new(BufWriter::new(File::create(path)?), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?;
    let mut stream = writer.stream_writer()?;
    let mut row = vec![0; width as usize * 4];
    for y in 0..height {
        for (x, rgba) in row.chunks_exact_mut(4).enumerate() {
            let [b, g, r, a] = fixture.pixel(x as u32, y);
            rgba.copy_from_slice(&[r, g, b, a]);
        }
        stream.write_all(&row)?;
    }
    stream.finish()?;
    writer.finish()?;
    Ok(())
}
struct Report {
    root: PathBuf,
    group: String,
    stages: Vec<Value>,
    groups: Vec<Value>,
    started: Instant,
    file: File,
    baseline: u64,
    q_max: u64,
    render_stage_private_max: u64,
}
impl Report {
    fn stage(&mut self, name: &str, detail: Value, render: bool) -> Result<()> {
        let memory = cpu_memory()?;
        if render {
            self.render_stage_private_max = self.render_stage_private_max.max(
                memory["private_bytes"]
                    .as_u64()
                    .ok_or("阶段内存缺少 private_bytes")?,
            );
        }
        let value = json!({"stage":name,"pid":std::process::id(),"unix_ms":unix_ms()?,"elapsed_ms":self.started.elapsed().as_secs_f64()*1000.,"render_phase":render,"cpu":memory,"gpu":gpu_memory(),"detail":detail});
        writeln!(self.file, "{value}")?;
        self.file.flush()?;
        self.stages.push(value);
        self.save(None, false)
    }
    fn render_limit(&self) -> u64 {
        self.baseline
            + u64::from(SHORT) * u64::from(LONG) * 4
            + 2 * self.q_max * 4
            + RENDER_ALLOWANCE
    }
    fn save(&self, error: Option<&str>, complete: bool) -> Result<()> {
        let value = json!({"suite":"image-preview","ok":complete&&error.is_none(),"error":error,"pid":std::process::id(),
            "requested_groups":[self.group],"groups":self.groups,"stages":self.stages,
            "baseline_private_bytes":self.baseline,"raw_bytes":u64::from(SHORT)*u64::from(LONG)*4,
            "actual_max_projected_pixels":self.q_max,"render_private_allowance_bytes":RENDER_ALLOWANCE,"render_private_limit_bytes":self.render_limit(),
            "render_stage_private_max":self.render_stage_private_max,"close_growth_limit_bytes":CLOSE_LIMIT,
            "clipboard_untouched":true,"global_input_sent":false,"desktop_pixels_read":false,"real_dxgi_capture":false,
            "browser_three_cycles_verified":false,"broader_goal_complete":false,
            "limitations":["自有 PNG/合成长图的真实 GPUI 编辑，未读桌面、剪贴板或用户窗口", "GPU 仅自有 framebuffer 稳定块，透明棋盘另按独立面积积分抽样，非GPU全图证明", "三轮有限关闭稳定性不代表长期无泄漏，GPU进程用量不是atlas直接驻留", "只覆盖当前实际DPI，没有混合DPI/负屏幕/真实浏览器采集结论", "外部100ms采样由runner记录，内部阶段样本不能称连续峰值"]});
        serde_json::to_writer_pretty(
            BufWriter::new(File::create(self.root.join("report.json"))?),
            &value,
        )?;
        Ok(())
    }
}

async fn presented<V: 'static + Render>(window: WindowHandle<V>, cx: &mut AsyncApp) -> Result<()> {
    let done = Rc::new(Cell::new(false));
    let signal = done.clone();
    window.update(cx, |_, w, _| {
        w.on_next_frame(move |w, _| {
            w.on_next_frame(move |_, _| signal.set(true));
            w.refresh();
        });
        w.refresh();
    })?;
    let end = Instant::now() + WAIT;
    while !done.get() {
        ensure(Instant::now() < end, "图片编辑实际两帧呈现超时")?;
        tick(cx).await;
    }
    Ok(())
}
async fn ready(window: ImageWindow, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        presented(window, cx).await?;
        if window.update(cx, |v, _, _| {
            !v.processing
                && !v.saving
                && v.status.is_empty()
                && v.previews.len() == 1
                && v.retired_raster_previews.is_empty()
                && v.retired_editor_preview_ids.is_empty()
        })? {
            return Ok(());
        }
        ensure(Instant::now() < end, "普通编辑器预览/退休没有收敛")?;
    }
}
async fn button(window: ImageWindow, id: &str, cx: &mut AsyncApp) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        let (p, input) = window.update(cx, |v, w, _| {
            (
                v.ui_positions.borrow().get(id).copied(),
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
            )
        })?;
        if let Some(p) = p {
            input.click(p);
            tick(cx).await;
            presented(window, cx).await?;
            return Ok(());
        }
        ensure(
            Instant::now() < end,
            &format!("实际图片编辑按钮不可达：{id}"),
        )?;
        tick(cx).await;
    }
}
fn image_client(window: ImageWindow, cx: &mut AsyncApp) -> Result<(Input, (u32, u32))> {
    window
        .update(cx, |_, w, _| {
            let dpi = w.scale_factor();
            let size = w.viewport_size();
            (
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: dpi,
                },
                (
                    (f32::from(size.width) * dpi).round() as u32,
                    (f32::from(size.height) * dpi).round() as u32,
                ),
            )
        })
        .map_err(Into::into)
}
fn middle_drag(input: Input, a: (u32, u32), b: (u32, u32)) -> Result<()> {
    let pack = |p: (u32, u32)| LPARAM(((p.1 << 16) | p.0) as isize);
    unsafe {
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            WM_MBUTTONDOWN,
            WPARAM(16),
            pack(a),
        )?;
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            WM_MOUSEMOVE,
            WPARAM(SMOKE_MOVE | 16),
            pack(b),
        )?;
        PostMessageW(
            Some(HWND(input.handle as *mut _)),
            WM_MBUTTONUP,
            WPARAM(0),
            pack(b),
        )?;
    }
    Ok(())
}
async fn center_at(
    window: ImageWindow,
    fixture: Fixture,
    long: f32,
    cx: &mut AsyncApp,
) -> Result<()> {
    let end = Instant::now() + WAIT;
    loop {
        let (input, host) = image_client(window, cx)?;
        let anchor = (host.0 / 2, host.1 / 2 - 20);
        let delta = window.update(cx, |v, _, _| -> Result<_> {
            let viewport = v.image_canvas.as_ref().ok_or("缺少普通图片视口")?.viewport;
            let at = Point {
                x: anchor.0 as f32,
                y: anchor.1 as f32,
            };
            let source = viewport.image_point(at);
            Ok(((if fixture.horizontal {
                source.x
            } else {
                source.y
            }) - long)
                * viewport.zoom)
        })??;
        if delta.abs() <= 1. {
            break;
        }
        let shift = delta.clamp(-120., 120.).round() as i32;
        let target = if fixture.horizontal {
            ((anchor.0 as i32 + shift) as u32, anchor.1)
        } else {
            (anchor.0, (anchor.1 as i32 + shift) as u32)
        };
        middle_drag(input, anchor, target)?;
        tick(cx).await;
        ensure(
            Instant::now() < end,
            "真实中键平移未把8192世界坐标移入可见区",
        )?;
    }
    ready(window, cx).await
}
fn world_client(
    window: ImageWindow,
    p: (u32, u32),
    cx: &mut AsyncApp,
) -> Result<(Input, (u32, u32))> {
    window.update(cx, |v, w, _| -> Result<_> {
        let viewport = v.image_canvas.as_ref().ok_or("缺少图片视口")?.viewport;
        let x = viewport.offset.x + p.0 as f32 * viewport.zoom;
        let y = viewport.offset.y + p.1 as f32 * viewport.zoom;
        let host = v.image_canvas.as_ref().ok_or("缺少图片视口")?.host;
        ensure(
            x > 8.
                && x < host.0 as f32 - 8.
                && y > 140. * w.scale_factor()
                && y < host.1 as f32 - 180. * w.scale_factor(),
            "真实标注端点超出自有可绘区域",
        )?;
        Ok((
            Input {
                handle: hwnd(w).0 as usize,
                scale: w.scale_factor(),
            },
            (x.round() as u32, y.round() as u32),
        ))
    })?
}
async fn stroke(
    window: ImageWindow,
    a: (u32, u32),
    b: (u32, u32),
    cx: &mut AsyncApp,
) -> Result<()> {
    let (input, a) = world_client(window, a, cx)?;
    let (_, b) = world_client(window, b, cx)?;
    input.stroke(&[a, b])?;
    tick(cx).await;
    ready(window, cx).await
}

fn integral_pixel(fixture: Fixture, gx: u32, gy: u32, density: f64) -> [u8; 4] {
    let (w, h) = fixture.dimensions();
    let left = (f64::from(gx) / density).min(f64::from(w));
    let right = (f64::from(gx + 1) / density).min(f64::from(w));
    let top = (f64::from(gy) / density).min(f64::from(h));
    let bottom = (f64::from(gy + 1) / density).min(f64::from(h));
    let mut sum = [0.; 3];
    for y in top.floor() as u32..bottom.ceil() as u32 {
        for x in left.floor() as u32..right.ceil() as u32 {
            let area = (right.min(f64::from(x + 1)) - left.max(f64::from(x)))
                * (bottom.min(f64::from(y + 1)) - top.max(f64::from(y)));
            let pixel = fixture.matte(x, y);
            for c in 0..3 {
                sum[c] += f64::from(pixel[c]) * area;
            }
        }
    }
    let area = (right - left) * (bottom - top);
    [0, 1, 2, 3].map(|c| {
        if c == 3 {
            255
        } else {
            (sum[c] / area).round().clamp(0., 255.) as u8
        }
    })
}
struct Trace {
    raw: Weak<RenderImage>,
    previews: Vec<Weak<RenderImage>>,
    identity: Option<SourceIdentity>,
    raw_pointer: usize,
    raw_id: usize,
}
fn source_probe(
    window: ImageWindow,
    fixture: Fixture,
    input: Option<ImageInputProbe>,
    trace: &mut Trace,
    all_raw: bool,
    cx: &mut AsyncApp,
) -> Result<(Value, u64)> {
    window.update(cx, |v, w, _| -> Result<_> {
        ensure(v.source.is_projected_image() && v.previews.len()==1 && v.source.screen_count()==1, "长图来源不是单一共享原图/单活动投影")?;
        let (raw,region,grid,density) = v.source.projected_image_for_qa().ok_or("没有共享投影元数据")?;
        let pixels = raw.as_bytes(0).ok_or("完整原图没有字节")?;
        let (width,height) = fixture.dimensions();
        ensure(v.source.full()==Rect::full(width,height) && v.source.original_rect(0)==Rect::full(width,height), "显示ROI污染完整原图坐标")?;
        ensure(pixels.len()==width as usize*height as usize*4 && raw.frame_count()==1, "完整原图字节/帧数错误")?;
        if let Some(input) = input { ensure(input.dimensions==(width,height) && input.input_pointer==pixels.as_ptr() as usize && input.input_length==pixels.len(), "解码器到RenderImage没有移动原始Vec")?; }
        if trace.raw_id==usize::MAX { trace.raw=Arc::downgrade(raw); trace.raw_id=raw.id.0; trace.raw_pointer=pixels.as_ptr() as usize; }
        ensure(trace.raw_id==raw.id.0 && trace.raw_pointer==pixels.as_ptr() as usize, "普通缩放/平移替换了完整原图身份")?;
        if all_raw { fixture.verify(Rect::full(width,height),pixels)?; }
        let identity = SourceIdentity::of(&v.source_pixels());
        if let Some(old) = &trace.identity { ensure(old==&identity, "可见区换图使Scene冻结原图identity变化")?; } else { trace.identity=Some(identity); }
        let preview = &v.previews[0];
        ensure(!Arc::ptr_eq(raw,preview) && preview.frame_count()==1 && (u32::from(preview.size(0).width),u32::from(preview.size(0).height))==(grid.width,grid.height), "投影尺寸或CPUraw进入绘制列表")?;
        ensure(grid.width<=16384 && grid.height<=16384 && u64::from(grid.width)*u64::from(grid.height)<=8_000_000, "实际投影超出单边/像素预算")?;
        let bytes = preview.as_bytes(0).ok_or("活动投影没有像素")?;
        ensure(bytes.len()==grid.width as usize*grid.height as usize*4, "投影字节长度错误")?;
        let mut integral_samples=Vec::new();
        for (x,y) in [(0,0),(grid.width/2,grid.height/2),(grid.width-1,grid.height-1),(grid.width-1,grid.height/2)] {
            let expected=integral_pixel(fixture,grid.x+x,grid.y+y,density);
            let offset=(y as usize*grid.width as usize+x as usize)*4;
            ensure(bytes[offset..offset+4]==expected, "原图棋盘/面积平均/小数全局grid相位与独立积分错误")?;
            integral_samples.push(json!({"global_pixel":[grid.x+x,grid.y+y],"actual_bgra":&bytes[offset..offset+4],"expected_bgra":expected}));
        }
        trace.previews.push(Arc::downgrade(preview));
        ensure(v.retired_raster_previews.is_empty() && v.retired_editor_preview_ids.is_empty(), "呈现后仍保留旧底图退休引用")?;
        Ok((json!({"raw_pointer":pixels.as_ptr() as usize,"raw_length":pixels.len(),"raw_image_id":raw.id.0,"input_probe":input,
            "single_shared_raw":true,"single_active_preview":true,"raw_not_gpu_preview":true,"raw_capacity_measured_after_close":true,
            "full":[width,height],"preview_region":[region.x,region.y,region.width,region.height],"grid":[grid.x,grid.y,grid.width,grid.height],"density":density,
            "preview_id":preview.id.0,"preview_length":bytes.len(),"source_identity_unchanged":true,"all_raw_bgra_alpha_checked":all_raw,
            "actual_scale_factor":w.scale_factor(),"physical_client_size":[(f32::from(w.viewport_size().width)*w.scale_factor()).round() as u32,(f32::from(w.viewport_size().height)*w.scale_factor()).round() as u32],
            "editor_preview_updates":v.editor_preview_updates,"editor_preview_releases":v.editor_preview_releases,
            "retired_raster_previews":v.retired_raster_previews.len(),"retired_editor_preview_ids":v.retired_editor_preview_ids.len(),"integral_samples":integral_samples}),u64::from(grid.width)*u64::from(grid.height)))
    })?
}

async fn gpu_blocks(
    window: ImageWindow,
    fixture: Fixture,
    cx: &mut AsyncApp,
) -> Result<Vec<Value>> {
    let (input, host_size) = image_client(window, cx)?;
    input.hover((host_size.0.saturating_sub(8), 8))?;
    presented(window, cx).await?;
    let (viewport, host, placement) = window.update(cx, |v, w, _| -> Result<_> {
        let state = v.image_canvas.as_ref().ok_or("没有图片视口")?;
        let mut origin = POINT::default();
        unsafe {
            ClientToScreen(hwnd(w), &mut origin).ok()?;
        }
        Ok((
            state.viewport,
            state.host,
            crate::pin_model::Placement {
                x: origin.x,
                y: origin.y,
                width: state.host.0,
                height: state.host.1,
            },
        ))
    })??;
    let work = crate::pin_native::work_area((placement.x, placement.y));
    let mut samples = Vec::new();
    // 从真实可见长轴中心选一个完整1024px均色band；期望仍独立来自源世界公式。
    let center = viewport.image_point(Point {
        x: host.0 as f32 / 2.,
        y: host.1 as f32 / 2. - 20.,
    });
    let visible_long = if fixture.horizontal {
        center.x
    } else {
        center.y
    };
    let band = (visible_long.clamp(0., (LONG - 1) as f32) as u32 / 1024).min(30);
    let local = visible_long - band as f32 * 1024.;
    let center = [256, 512, 768]
        .into_iter()
        .min_by(|a, b| {
            (local - *a as f32)
                .abs()
                .total_cmp(&(local - *b as f32).abs())
        })
        .ok_or("没有稳定颜色块锚点")?;
    let long = band * 1024 + center;
    for short in [240, 680] {
        let (sx, sy) = fixture.xy(short, long);
        let px = (f64::from(viewport.offset.x) + (f64::from(sx) + 0.5) * f64::from(viewport.zoom))
            .floor() as u32;
        let py = (f64::from(viewport.offset.y) + (f64::from(sy) + 0.5) * f64::from(viewport.zoom))
            .floor() as u32;
        let context=format!("src=({sx},{sy}), physical=({px},{py}), viewport={viewport:?}, host={host:?}, placement={placement:?}, work={work:?}");
        ensure(
            px >= 4 && py >= 4 && px + 4 < host.0 && py + 4 < host.1,
            &format!("图片编辑GPU稳定块不在客户区：{context}"),
        )?;
        let rect = [px - 1, py - 1, 3, 3];
        ensure(
            i64::from(placement.x) + i64::from(rect[0]) >= i64::from(work.x)
                && i64::from(placement.y) + i64::from(rect[1]) >= i64::from(work.y)
                && i64::from(placement.x) + i64::from(rect[0] + 3)
                    <= i64::from(work.x) + i64::from(work.width)
                && i64::from(placement.y) + i64::from(rect[1] + 3)
                    <= i64::from(work.y) + i64::from(work.height),
            "图片编辑自有GPU样本不在可见工作区",
        )?;
        let ratio = f64::from(viewport.zoom);
        let density = window
            .update(cx, |v, _, _| {
                v.source
                    .projected_image_for_qa()
                    .map(|(_, _, _, density)| density)
            })?
            .ok_or("GPU采样没有实际投影密度")?;
        // 3×3物理样本的完整反投影，再扩一个投影cell以覆盖LINEAR相邻texel的Box面积支持；另加2源像素覆盖取整边界。
        let margin = 1. / density + 2.;
        let footprint = [
            ((f64::from(rect[0]) - f64::from(viewport.offset.x)) / ratio).floor() - margin,
            ((f64::from(rect[1]) - f64::from(viewport.offset.y)) / ratio).floor() - margin,
            ((f64::from(rect[0] + 3) - f64::from(viewport.offset.x)) / ratio).ceil() + margin,
            ((f64::from(rect[1] + 3) - f64::from(viewport.offset.y)) / ratio).ceil() + margin,
        ];
        let (w, h) = fixture.dimensions();
        ensure(
            footprint[0] >= 0.
                && footprint[1] >= 0.
                && footprint[2] <= f64::from(w)
                && footprint[3] <= f64::from(h),
            &format!("图片编辑GPU过滤裕量超出原图：{footprint:?}；{context}"),
        )?;
        let expected = fixture.pixel(sx, sy);
        let [x0, y0, x1, y1] = footprint.map(|n| n as u32);
        for y in y0..y1 {
            for x in x0..x1 {
                ensure(
                    fixture.pixel(x, y) == expected && expected[3] == 255,
                    &format!("图片编辑GPU过滤裕量穿过颜色/alpha边界：{footprint:?}；{context}"),
                )?;
            }
        }
        window.update(cx, |_, w, _| w.request_frame_capture(rect))??;
        let end = Instant::now() + WAIT;
        let bytes = loop {
            tick(cx).await;
            if let Some(capture) = window.update(cx, |_, w, _| w.take_frame_capture())? {
                break capture.map_err(|e| e.to_string())?;
            }
            ensure(Instant::now() < end, "图片编辑自有GPU读回超时")?;
        };
        ensure(bytes.len() == 36, "图片编辑GPU小样字节数错误")?;
        let difference = bytes
            .chunks_exact(4)
            .flat_map(|p| p.iter().zip(expected).map(|(&a, b)| a.abs_diff(b)))
            .max()
            .unwrap_or(0);
        ensure(difference<=1,&format!("图片编辑GPU旧缓存或颜色错误：{difference}, actual={bytes:?}, expected={expected:?}；{context}"))?;
        samples.push(json!({"source_anchor":[sx,sy],"physical_rect":rect,"source_footprint":[x0,y0,x1,y1],"expected_bgra":expected,"actual_bgra":bytes,"max_difference":difference,"tolerance":1,"owned_framebuffer":true}));
    }
    Ok(samples)
}

fn adapter_stats(stats: &AdapterStats) -> Value {
    json!({"keys":stats.keys.get(),"restored":stats.restored.get(),"errors":stats.errors.get()})
}
// 原有adapter仅在自有HWND的UI线程临时保存/恢复keyboardstate；它并不发送全局输入。
async fn current_shortcuts(
    window: ImageWindow,
    fixture: Fixture,
    stats: &Rc<AdapterStats>,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let mut events = Vec::new();
    for control in [false, true] {
        button(window, "crop", cx).await?;
        button(window, "more", cx).await?;
        button(window, "more-actions-tab", cx).await?;
        button(window, "reselect", cx).await?;
        stroke(window, fixture.xy(300, 8128), fixture.xy(500, 8256), cx).await?;
        // 使Ctrl+A进入真实“当前选区”分支：非Edit工具且选区先不是full。
        button(window, "rectangle", cx).await?;
        let (before, full, identity, input) = window.update(cx, |v, w, _| -> Result<_> {
            let before = v.editor.crop.ok_or("A快捷键前没有真实小选区")?;
            ensure(
                before != v.source.full() && v.editor.tool != Tool::Edit,
                "A快捷键前置选区/工具没有实际改变",
            )?;
            Ok((
                before,
                v.source.full(),
                SourceIdentity::of(&v.source_pixels()),
                Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                },
            ))
        })??;
        let previous = stats.keys.get();
        text_contract_smoke::key(input, VIRTUAL_KEY(0x41), control, false, false)?;
        let end = Instant::now() + WAIT;
        loop {
            tick(cx).await;
            if stats.keys.get() > previous {
                break;
            }
            ensure(Instant::now() < end, "自有tagged A消息未进入原生适配器")?;
        }
        ready(window, cx).await?;
        let after = window.update(cx, |v, _, _| -> Result<_> {
            ensure(
                v.editor.crop == Some(full),
                "native A/Ctrl+A把完整图误选为当前显示ROI",
            )?;
            ensure(
                SourceIdentity::of(&v.source_pixels()) == identity,
                "native A/Ctrl+A替换了完整原图来源",
            )?;
            let (x, y) = fixture.xy(900, 8192);
            let output = v.source.read_region(Rect {
                x,
                y,
                width: 1,
                height: 1,
            })?;
            ensure(
                output.bgra == fixture.pixel(x, y),
                "native A/Ctrl+A后原图世界像素不正确",
            )?;
            Ok(v.editor.crop.unwrap())
        })??;
        ensure(
            stats.errors.get() == 0 && stats.keys.get() == stats.restored.get(),
            "native A adapter没有实际完整保存/恢复UI线程键盘状态",
        )?;
        events.push(json!({"key":if control{"Ctrl+A"}else{"A"},"before_crop":[before.x,before.y,before.width,before.height],"after_crop":[after.x,after.y,after.width,after.height],"tool_non_edit":true,"source_identity_unchanged":true}));
    }
    Ok(
        json!({"adapter_scope":"tagged-own-HWND-native-key/UI-thread-keyboard-state-save-restore","events":events,"adapter_stats":adapter_stats(stats),"global_input_sent":false,"hardware_keyboard_tested":false}),
    )
}

#[allow(clippy::too_many_arguments)]
async fn checkpoint(
    report: &mut Report,
    window: ImageWindow,
    fixture: Fixture,
    input: Option<ImageInputProbe>,
    trace: &mut Trace,
    round: u32,
    name: &str,
    all_raw: bool,
    cx: &mut AsyncApp,
) -> Result<()> {
    ready(window, cx).await?;
    let (source, q) = source_probe(window, fixture, input, trace, all_raw, cx)?;
    report.q_max = report.q_max.max(q);
    let samples = gpu_blocks(window, fixture, cx).await?;
    report.stage(name,json!({"round":round,"source":source,"gpu_stable_blocks":samples,"active_projected_pixels":q}),name!="file-refit")
}

async fn annotations(window: ImageWindow, fixture: Fixture, cx: &mut AsyncApp) -> Result<Value> {
    button(window, "rectangle", cx).await?;
    let filled = window.update(cx, |v, _, _| v.editor.style.filled)?;
    if !filled {
        button(window, "filled", cx).await?;
    }
    stroke(window, fixture.xy(300, 8064), fixture.xy(500, 8320), cx).await?;
    let rectangle = window
        .update(cx, |v, _, _| v.editor.annotations().first().cloned())?
        .ok_or("真实矩形手势未提交")?;
    ensure(
        rectangle.tool == Tool::Rectangle && rectangle.style.filled,
        "真实矩形填充设置未提交",
    )?;
    let color = [
        rectangle.style.color as u8,
        (rectangle.style.color >> 8) as u8,
        (rectangle.style.color >> 16) as u8,
        255,
    ];
    let (sx, sy) = fixture.xy(432, 8192);
    window.update(cx, |v, _, _| -> Result<()> {
        let r = Rect {
            x: sx,
            y: sy,
            width: 1,
            height: 1,
        };
        let mut image = v.source.read_region(r)?;
        v.prepared_scene().apply(&mut image, r);
        ensure(image.bgra == color, "跨8192实心矩形中心不是预期颜色")
    })??;
    button(window, "mosaic", cx).await?;
    stroke(window, fixture.xy(400, 8160), fixture.xy(496, 8208), cx).await?;
    let bs = window.update(cx, |v, _, _| -> Result<_> {
        ensure(
            v.editor.annotations().len() == 2,
            "真实矩形/马赛克手势未产生两对象",
        )?;
        let annotations = v.editor.annotations();
        let bs = annotations[1].style.block_size.clamp(2, 64);
        let scene = v.prepared_scene();
        ensure(scene.errors.is_empty(), "Scene效果准备失败")?;
        ensure(
            scene
                .layers
                .iter()
                .any(|l| matches!(l, Layer::Mosaic { .. })),
            "没有实际Scene马赛克层",
        )?;
        Ok(bs)
    })??;
    // 独立平均一个完整网格 cell；测试点的下层均为实心矩形，避免以Scene输出作期望。
    let check = |v: &mut CaptureView| -> Result<()> {
        let r = Rect {
            x: sx,
            y: sy,
            width: 1,
            height: 1,
        };
        let mut output = v.source.read_region(r)?;
        v.prepared_scene().apply(&mut output, r);
        ensure(output.bgra == color, "马赛克没有从下层实心矩形采样")
    };
    window.update(cx, |v, _, _| check(v))??;
    button(window, "undo", cx).await?;
    ensure(
        window.update(cx, |v, _, _| v.editor.annotations().len())? == 1,
        "一次Undo未只撤马赛克",
    )?;
    button(window, "redo", cx).await?;
    window.update(cx, |v, _, _| check(v))??;
    // 缓存基准在Redo之后取；平移只改变显示ROI，不应重新烘焙下层效果。
    let cache = window.update(cx, |v, _, _| -> Result<_> {
        let scene = v.prepared_scene();
        scene
            .layers
            .iter()
            .find_map(|l| {
                if let Layer::Mosaic { blocks } = l {
                    Some(Arc::downgrade(blocks))
                } else {
                    None
                }
            })
            .ok_or_else(|| "Redo没有Scene马赛克缓存".into())
    })??;
    let (input, host) = image_client(window, cx)?;
    let a = (host.0 / 2, host.1 / 2 - 20);
    middle_drag(input, a, (a.0 + 7, a.1 + 5))?;
    tick(cx).await;
    ready(window, cx).await?;
    window.update(cx, |v, _, _| -> Result<()> {
        let old = cache.upgrade().ok_or("仅平移即丢失Scene马赛克缓存")?;
        let scene = v.prepared_scene();
        let current = scene
            .layers
            .iter()
            .find_map(|l| {
                if let Layer::Mosaic { blocks } = l {
                    Some(blocks)
                } else {
                    None
                }
            })
            .ok_or("平移后无Scene马赛克层")?;
        ensure(Arc::ptr_eq(&old, current), "仅ROI平移重新采样Scene原图")?;
        check(v)
    })??;
    drop(cache);
    Ok(
        json!({"rectangle_across_8192":true,"rectangle_filled":true,"mosaic_lower_layer_sample_correct":true,"mosaic_block_size":bs,"undo_redo_real_buttons":true,"scene_cache_preserved_across_pan":true,"point":[sx,sy],"expected_bgra":color}),
    )
}

async fn crop_export(window: ImageWindow, fixture: Fixture, cx: &mut AsyncApp) -> Result<Value> {
    let annotation_count = window.update(cx, |v, _, _| v.editor.annotations().len())?;
    button(window, "crop", cx).await?;
    button(window, "more", cx).await?;
    button(window, "more-actions-tab", cx).await?;
    button(window, "reselect", cx).await?;
    ensure(
        window.update(cx, |v, _, _| v.editor.crop.is_none())?,
        "真实重选没有清空选区",
    )?;
    stroke(window, fixture.xy(888, 8140), fixture.xy(948, 8270), cx).await?;
    let result=window.update(cx,|v,_,_|->Result<Value> {let rect=v.editor.crop.ok_or("真实crop手势未提交")?; ensure(rect.width>0 && rect.height>0,"真实crop为空")?; let output=v.selected_content_image()?; ensure((output.width,output.height)==(rect.width,rect.height),"crop导出尺寸使用了预览")?; fixture.verify(rect,&output.bgra)?; let alphas=output.bgra.chunks_exact(4).fold([false;5],|mut seen,p|{if let Some(i)=[0,64,128,192,255].iter().position(|a|*a==p[3]){seen[i]=true;}seen}); ensure(!fixture.alpha || alphas.iter().all(|v|*v),"alpha crop没有保留零/半透明/不透明及隐藏RGB")?; ensure(v.editor.annotations().len()==annotation_count,"重选crop清除了世界标注")?; Ok(json!({"actual_crop":[rect.x,rect.y,rect.width,rect.height],"output_dimensions":[output.width,output.height],"all_bgra_alpha_correct":true,"alpha_values_seen":alphas,"annotations_preserved":true,"annotation_count":annotation_count,"clipboard_written":false}))})??;
    button(window, "image-full", cx).await?;
    ensure(
        window.update(cx, |v, _, _| v.editor.crop == Some(v.source.full()))?,
        "真实完整图片按钮没有恢复full crop",
    )?;
    Ok(result)
}

async fn close_image(
    window: ImageWindow,
    input: Option<ImageInputProbe>,
    trace: &Trace,
    cx: &mut AsyncApp,
) -> Result<Value> {
    // 仅到关闭阶段才持这一份强引用；render/处理内存阶段只保存Weak和数字。
    let (raw, handle) = window.update(cx, |v, w, _| -> Result<_> {
        let (raw, _, _, _) = v.source.projected_image_for_qa().ok_or("关闭时原图缺失")?;
        Ok((raw.clone(), hwnd(w).0 as usize))
    })??;
    unsafe {
        PostMessageW(Some(HWND(handle as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0))?;
    }
    let end = Instant::now() + WAIT;
    loop {
        tick(cx).await;
        let absent = cx.update(|cx| {
            cx.windows()
                .into_iter()
                .all(|w| w.downcast::<CaptureView>().is_none())
        })?;
        if absent && !unsafe { IsWindow(Some(HWND(handle as *mut _))) }.as_bool() {
            break;
        }
        ensure(Instant::now() < end, "普通编辑器实体/原始HWND没有实际销毁")?;
    }
    let image = Arc::try_unwrap(raw).map_err(|_| "窗口销毁后完整原图还有未知强持有者")?;
    let frame = image
        .try_into_single_frame()
        .map_err(|_| "完整原图不是可消费单帧")?;
    let buffer = frame.buffer().as_raw();
    let pointer = buffer.as_ptr() as usize;
    let length = buffer.len();
    let capacity = buffer.capacity();
    ensure(
        pointer == trace.raw_pointer,
        "关闭后实际Vec不是同一raw pointer",
    )?;
    if let Some(input) = input {
        ensure(
            (pointer, length, capacity)
                == (
                    input.input_pointer,
                    input.input_length,
                    input.input_capacity,
                ),
            "解码Vec pointer/len/capacity移交发生额外原图复制",
        )?;
    }
    drop(frame);
    while trace.raw.upgrade().is_some() || trace.previews.iter().any(|w| w.upgrade().is_some()) {
        ensure(
            Instant::now() < end,
            "真正Drop Frame后完整原图或旧投影Weak仍存活",
        )?;
        tick(cx).await;
    }
    ensure(
        crate::long_capture_runtime::active_workers() == 0
            && crate::long_capture_pipeline::active_capture_threads() == 0,
        "编辑器关闭后仍有消费者/采集线程",
    )?;
    cx.background_executor()
        .timer(Duration::from_millis(500))
        .await;
    let memory = cpu_memory()?;
    Ok(
        json!({"hwnd":handle,"entity_removed":true,"owned_hwnd_destroyed":true,"raw_frame_dropped_before_weak_check":true,"raw_weak_expired":true,
        "preview_weaks_expired":trace.previews.len(),"actual_raw_pointer":pointer,"actual_raw_length":length,"actual_raw_capacity":capacity,
        "input_vec_moved_without_tile_copy":input.is_some(),"input_probe_available":input.is_some(),"private_bytes_after_500ms":memory["private_bytes"],"capture_workers":0,"consumer_sessions":0}),
    )
}

async fn file_cycles(report: &mut Report, fixture: Fixture, cx: &mut AsyncApp) -> Result<()> {
    let path = report.root.join(format!("owned-{}.png", report.group));
    let file = path.clone();
    cx.background_executor()
        .spawn(async move { write_fixture(&file, fixture).map_err(|e| e.to_string()) })
        .await
        .map_err(|e| e.to_string())?;
    let mut closed = Vec::new();
    for round in 1..=3 {
        report.stage(
            "file-render-begin",
            json!({"round":round,"path":path,"fixture_generated_by_single_rgba_row":true}),
            true,
        )?;
        let (window, input) = crate::image_editor::open_file(path.clone(), cx).await?;
        window.update(cx, |_, w, _| crate::gpui_smoke::isolate_input(hwnd(w)))??;
        let stats = Rc::new(AdapterStats::default());
        let (native, _) = image_client(window, cx)?;
        text_contract_smoke::install(native, &stats)?;
        let mut trace = Trace {
            raw: Weak::new(),
            previews: Vec::new(),
            identity: None,
            raw_pointer: 0,
            raw_id: usize::MAX,
        };
        checkpoint(
            report,
            window,
            fixture,
            Some(input),
            &mut trace,
            round,
            "file-fit",
            true,
            cx,
        )
        .await?;
        button(window, "image-actual", cx).await?;
        ensure(
            window.update(cx, |v, _, _| {
                v.image_canvas
                    .as_ref()
                    .is_some_and(|s| (s.viewport.zoom - 1.).abs() < 0.0001)
            })?,
            "实际100%按钮没有改变视口",
        )?;
        center_at(window, fixture, 8192., cx).await?;
        checkpoint(
            report,
            window,
            fixture,
            Some(input),
            &mut trace,
            round,
            "file-actual-pan-8192",
            false,
            cx,
        )
        .await?;
        let shortcuts = current_shortcuts(window, fixture, &stats, cx).await?;
        report.stage("file-current-shortcuts",json!({"round":round,"phase":"actual-pan","adapter_scope":shortcuts["adapter_scope"],"events":shortcuts["events"],"adapter_stats":shortcuts["adapter_stats"]}),true)?;
        for _ in 0..5 {
            button(window, "image-zoom-out", cx).await?;
        }
        center_at(window, fixture, 8192., cx).await?;
        checkpoint(
            report,
            window,
            fixture,
            Some(input),
            &mut trace,
            round,
            "file-fractional-pan",
            false,
            cx,
        )
        .await?;
        let shortcuts = current_shortcuts(window, fixture, &stats, cx).await?;
        report.stage("file-current-shortcuts",json!({"round":round,"phase":"fractional-pan","adapter_scope":shortcuts["adapter_scope"],"events":shortcuts["events"],"adapter_stats":shortcuts["adapter_stats"]}),true)?;
        let mark = annotations(window, fixture, cx).await?;
        checkpoint(
            report,
            window,
            fixture,
            Some(input),
            &mut trace,
            round,
            "file-scene-pan",
            true,
            cx,
        )
        .await?;
        report.stage("file-render-end",json!({"round":round,"annotations":mark,"actual_q_max":report.q_max,"render_limit_bytes":report.render_limit()}),true)?;
        let crop = crop_export(window, fixture, cx).await?;
        report.stage(
            "file-alpha-crop-export",
            json!({"round":round,"result":crop,"excluded_from_render_peak":true}),
            false,
        )?;
        let full = window.update(cx, |v, _, _| -> Result<_> {
            let output = v.selected_content_image()?;
            ensure(
                (output.width, output.height) == fixture.dimensions(),
                "恢复full后的生产导出仍使用显示ROI尺寸",
            )?;
            Ok((output.width, output.height))
        })??;
        report.stage("file-full-export",json!({"round":round,"dimensions":full,"full_bgra_dimensions_verified":true,"excluded_from_render_peak":true}),false)?;
        button(window, "more", cx).await?;
        button(window, "more-actions-tab", cx).await?;
        button(window, "clear", cx).await?;
        ensure(
            window.update(cx, |v, _, _| v.editor.annotations().is_empty())?,
            "功能核验后实际清空按钮未清除标注",
        )?;
        button(window, "image-fit", cx).await?;
        checkpoint(
            report,
            window,
            fixture,
            Some(input),
            &mut trace,
            round,
            "file-refit",
            false,
            cx,
        )
        .await?;
        let close = close_image(window, Some(input), &trace, cx).await?;
        closed.push(
            close["private_bytes_after_500ms"]
                .as_u64()
                .ok_or("关闭样本缺少private")?,
        );
        report.stage(
            "file-round-closed",
            json!({"round":round,"detail":close}),
            false,
        )?;
    }
    let growth = closed
        .iter()
        .copied()
        .max()
        .ok_or("没有关闭样本")?
        .saturating_sub(closed[0]);
    report.stage("file-close-memory-gate",json!({"private_bytes":closed,"growth_bytes":growth,"limit_bytes":CLOSE_LIMIT,"ok":growth<=CLOSE_LIMIT}),false)?;
    ensure(
        growth <= CLOSE_LIMIT,
        "普通长图文件编辑器三轮close增长超过原16MiB",
    )?;
    ensure(
        report.render_stage_private_max <= report.render_limit(),
        "普通长图渲染阶段private样本超过预登记 R+2Q+96MiB",
    )?;
    report.groups.push(json!({"name":format!("three-rounds-32000px-image-preview-{}",report.group),"ok":true,"rounds":3,"source_dimensions":fixture.dimensions(),"alpha":fixture.alpha,"horizontal":fixture.horizontal,
        "real_file_decode_and_editor_open":true,"input_vec_move_verified":true,"full_raw_bgra_alpha_verified":true,"no_tiles":true,"single_active_preview":true,
        "real_ui_zoom_pan_rectangle_effect_crop":true,"source_identity_preserved":true,"gpu_stable_blocks_per_checkpoint":2,"gpu_all_pixels_verified":false,
        "native_current_shortcut_events":12,"native_current_shortcuts_verified":true,"native_keyboard_adapter_scope":"tagged own HWND / UI thread keyboardstate saved and restored / no global input",
        "independent_projected_checker_integral_samples":true,"close_private_bytes":closed,"close_private_growth_bytes":growth,"close_private_limit_bytes":CLOSE_LIMIT,
        "raw_preview_weaks_expired":true,"owned_hwnds_destroyed":true,"capture_workers":0,"consumer_sessions":0,"render_private_limit_bytes":report.render_limit(),"actual_max_projected_pixels":report.q_max,
        "render_stage_private_max":report.render_stage_private_max,"external_sampled_peak_verified_by_runner":false}));
    report.save(None, false)
}

async fn long_annotate(report: &mut Report, cx: &mut AsyncApp) -> Result<()> {
    let fixture = Fixture {
        alpha: false,
        horizontal: false,
    };
    let (snapshot, old) = cx
        .background_executor()
        .spawn(async move {
            let mut bytes = Vec::with_capacity(SHORT as usize * LONG as usize * 4);
            for y in 0..LONG {
                for x in 0..SHORT {
                    bytes.extend_from_slice(&fixture.pixel(x, y));
                }
            }
            let image = Arc::new(Image {
                width: SHORT,
                height: LONG,
                bgra: bytes,
                readback_ms: 0.,
            });
            let weak = Arc::downgrade(&image);
            (
                StitchSnapshot {
                    width: SHORT,
                    height: LONG,
                    axis: ScrollAxis::Vertical,
                    viewport: Rect::full(SHORT, 640),
                    segments: vec![StitchSegment {
                        image,
                        source: Rect::full(SHORT, LONG),
                        origin: (0, 0),
                    }],
                },
                weak,
            )
        })
        .await;
    // 生产 Crop 物化一次后，剪辑与原生标注共享同一个 CaptureView。
    let completed = Rc::new(Cell::new(false));
    let signal = completed.clone();
    let product = cx
        .update(|cx| {
            crate::long_output::export(snapshot, crate::long_output::LongOutput::Crop, cx)
        })?
        .await?;
    let crate::long_output::LongProduct::EditorImage { image, axis } = product else {
        return Err("32000px Crop 没有进入同窗图片编辑器产品".into());
    };
    ensure(old.upgrade().is_none(), "Crop 物化后旧采集条带未释放")?;
    let window = cx.update(|cx| {
        crate::image_editor::open_long(
            image,
            axis,
            None,
            move |outcome, _| {
                signal.set(matches!(
                    outcome,
                    crate::long_output::LongOutcome::Return(_)
                ));
            },
            cx,
        )
    })??;
    let handle = window.update(cx, |v, w, _| -> Result<_> {
        ensure(
            v.editor.long_clip().is_some() && v.long_clip_ui.is_some(),
            "open_long 没有初始化同窗剪辑",
        )?;
        Ok(hwnd(w).0 as usize)
    })??;
    window.update(cx, |_, w, _| crate::gpui_smoke::isolate_input(hwnd(w)))??;
    crate::long_editor_smoke::prepare_clip_input(window, "image-preview-long", cx).await?;
    let mut trace = Trace {
        raw: Weak::new(),
        previews: Vec::new(),
        identity: None,
        raw_pointer: 0,
        raw_id: usize::MAX,
    };
    checkpoint(
        report,
        window,
        fixture,
        None,
        &mut trace,
        1,
        "unified-long-fit",
        true,
        cx,
    )
    .await?;
    button(window, "long-clip-actual", cx).await?;
    center_at(window, fixture, 8192., cx).await?;
    let mark = annotations(window, fixture, cx).await?;
    checkpoint(
        report,
        window,
        fixture,
        None,
        &mut trace,
        1,
        "unified-long-scene-pan",
        true,
        cx,
    )
    .await?;
    let crop = window.update(cx, |v, w, cx| -> Result<Value> {
        let count = v.editor.annotations().len();
        let canvas = v.editor.canvas;
        ensure(hwnd(w).0 as usize == handle, "标注切换了长图宿主窗口")?;
        ensure(v.editor.trim_long_clip(8140..8270, 888..948)?, "长图四边裁剪没有提交")?;
        let output = v.selected_content_image()?;
        ensure((output.width, output.height) == (60, 130), "长图四边裁剪导出尺寸错误")?;
        fixture.verify(Rect { x: 888, y: 8140, width: 60, height: 130 }, &output.bgra)?;
        ensure(v.editor.annotations().len() == count && v.editor.canvas == canvas, "长图裁剪改变了原图标注或画布")?;
        ensure(v.editor.clear_long_clip()?, "长图四边裁剪不能恢复原图")?;
        cx.notify();
        Ok(json!({"actual_crop":[888,8140,60,130],"output_dimensions":[60,130],"all_bgra_alpha_correct":true,"annotations_preserved":true,"annotation_count":count,"same_native_window":true,"clipboard_written":false}))
    })??;
    // 正常关闭先触发未保存确认，再发送第二次关闭；不绕过生产丢弃保护。
    unsafe {
        PostMessageW(Some(HWND(handle as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0))?;
    }
    tick(cx).await;
    ensure(
        unsafe { IsWindow(Some(HWND(handle as *mut _))) }.as_bool(),
        "未保存长图首轮关闭没有保留确认机会",
    )?;
    let close = close_image(window, None, &trace, cx).await?;
    ensure(completed.get(), "关闭同窗长图没有回到原采集会话")?;
    report.stage("long-annotate-closed",json!({"close":close,"old_strip_weak_expired":old.upgrade().is_none(),"annotations":mark,"crop":crop}),false)?;
    report.groups.push(json!({"name":"long-editor-annotate-shared-image-preview","ok":true,"rounds":1,"production_crop_open_long":true,"same_window_annotation":true,"old_strip_weak_expired":true,"close_return_callback":true,
        "single_shared_raw":true,"full_raw_bgra_alpha_verified":true,"gpu_stable_blocks_verified":true,"raw_preview_weaks_expired":true,"owned_hwnd_destroyed":true,
        "real_scene_effect_crop":true,"decode_input_probe_available":false,"close_growth_three_rounds_tested":false,"handoff_overlap_excluded_from_file_render_peak":true,"capture_workers":0,"consumer_sessions":0}));
    report.save(None, false)
}
struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
    }
}
pub(crate) fn run() -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let root = std::env::var_os("RUSTSHOT_IMAGE_PREVIEW_QA_DIR")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?.join("target/qa/image-preview"));
    std::fs::create_dir_all(&root)?;
    let root = root.canonicalize()?;
    let allowed = std::env::current_dir()?.join("target/qa").canonicalize()?;
    ensure(
        root.starts_with(&allowed),
        "图片编辑QA产物必须位于当前target/qa",
    )?;
    let group = std::env::var("RUSTSHOT_IMAGE_PREVIEW_GROUPS").unwrap_or_else(|_| "opaque".into());
    ensure(
        matches!(
            group.as_str(),
            "opaque" | "alpha" | "horizontal" | "long-annotate" | "pin-edit"
        ),
        "图片预览QA每个冷进程必须只选择一个已知组",
    )?;
    std::fs::create_dir_all(root.join("settings"))?;
    std::env::set_var("RUSTSHOT_DATA_DIR", root.join("settings"));
    std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
    crate::preferences::Preferences {
        history: false,
        pin_restore: false,
        ..Default::default()
    }
    .save()?;
    let file = File::create(root.join("stages.jsonl"))?;
    let holder = Rc::new(RefCell::new(Some(Report {
        root,
        group,
        stages: Vec::new(),
        groups: Vec::new(),
        started: Instant::now(),
        file,
        baseline: 0,
        q_max: 0,
        render_stage_private_max: 0,
    })));
    let final_holder = holder.clone();
    let error = Rc::new(RefCell::new(Some("图片预览专项未完成".to_string())));
    let final_error = error.clone();
    Application::new().with_assets(crate::toolbar_icons::IconAssets).run(move|cx:&mut App|{
        let host=match cx.open_window(WindowOptions{show:false,focus:false,..Default::default()},|_,cx|cx.new(|_|Host)){Ok(h)=>h,Err(e)=>{*error.borrow_mut()=Some(e.to_string());cx.quit();return;}};
        cx.spawn(async move|cx|{
            let mut report=holder.borrow_mut().take().expect("独占图片预览QA报告");
            let result:Result<()>=async {
                // 隐藏keepalive宿主不提交帧；基线沿现有runtime专项等待UI timer，再直接测量真实进程用量。
                tick(cx).await;
                let host_handle=host.update(cx,|_,w,_|hwnd(w).0 as usize)?;
                report.baseline=cpu_memory()?["private_bytes"].as_u64().ok_or("隐藏Host基线private缺失")?;
                report.stage("hidden-own-host-baseline",json!({"no_image_source_allocated":true,"no_user_window_read":true,"baseline_private_bytes":report.baseline,
                    "host_hwnd":host_handle,"host_options_show":false,"host_options_focus":false,"host_frame_presentation_waited":false,"baseline_wait":"UI timer tick; hidden keepalive window does not submit frames"}),false)?;
                let selected_group=report.group.clone();
                match selected_group.as_str(){
                    "opaque"=>file_cycles(&mut report,Fixture{alpha:false,horizontal:false},cx).await?,
                    "alpha"=>file_cycles(&mut report,Fixture{alpha:true,horizontal:false},cx).await?,
                    "horizontal"=>file_cycles(&mut report,Fixture{alpha:false,horizontal:true},cx).await?,
                    "long-annotate"=>long_annotate(&mut report,cx).await?,
                    "pin-edit"=>pin_entry::run(&mut report,cx).await?,
                    _=>return Err("未知图片预览专项".into()),
                }
                Ok(())
            }.await;
            *error.borrow_mut()=result.err().map(|e|e.to_string());
            *holder.borrow_mut()=Some(report);
            let _=cx.update(|cx|{for w in cx.windows(){let _=cx.update_window(w,|_,w,_|w.remove_window());}cx.quit();});
        }).detach();
    });
    let error = final_error.borrow_mut().take();
    if let Some(mut report) = final_holder.borrow_mut().take() {
        report.stage("after-application",json!({"capture_workers":crate::long_capture_runtime::active_workers(),"capture_threads":crate::long_capture_pipeline::active_capture_threads()}),false)?;
        report.save(error.as_deref(), true)?;
    }
    if let Some(error) = error {
        Err(error.into())
    } else {
        Ok(())
    }
}
