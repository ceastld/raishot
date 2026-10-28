//! 缩放计时专项的合成帧检查（`RUSTSHOT_PIN_ZOOM_COMPOSITE_QA=1`）。
//! 后台线程用 DXGI 桌面复制取贴图所在显示器的每个合成帧，只复制穿过检查点的一行一列像素，
//! 不保存图像。每帧从检查点向外走出贴图外框，并找内容里的竖条／横条标记：
//! 外框不是缩放前后任一矩形，或标记相对外框的比例与首帧不符（旧倍率内容配新窗口或反之），
//! 都算一帧抖动。
use super::{ensure, qpc};
use crate::Result;
use std::sync::{
    atomic::{AtomicBool, AtomicI64, Ordering},
    Arc,
};
use windows::{
    core::Interface,
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
};

/// 标记竖条／横条在选区里的位置与宽度（比例）。检查点在左上 1/4 附近，与标记不相交。
pub(super) const BAR_FRACTION: f32 = 0.6;
pub(super) const BAR_WIDTH_FRACTION: f32 = 0.02;
pub(super) const BAR_COLOR: u32 = 0xd83030;

pub(super) fn enabled() -> bool {
    std::env::var("RUSTSHOT_PIN_ZOOM_COMPOSITE_QA").as_deref() == Ok("1")
}

pub(super) struct Frame {
    pub qpc: i64,
    pub accumulated: u32,
    /// 检查点（屏幕物理坐标）。
    pub point: (i32, i32),
    /// 穿过检查点的整行／整列 BGRA。
    row: Vec<[u8; 4]>,
    column: Vec<[u8; 4]>,
}

pub(super) struct Recording {
    pub frames: Vec<Frame>,
    /// 显示器桌面坐标 [left, top, right, bottom]。
    pub output: [i32; 4],
    /// 工作区：任务栏等置顶窗口会挡住贴图，外框只比较这一范围内的部分。
    pub work: [i32; 4],
    pub error: Option<String>,
}

pub(super) fn pack(point: (i32, i32)) -> i64 {
    (i64::from(point.0) << 32) | i64::from(point.1 as u32)
}
fn unpack(value: i64) -> (i32, i32) {
    ((value >> 32) as i32, value as u32 as i32)
}

fn output_at(point: (i32, i32)) -> Result<(IDXGIAdapter1, IDXGIOutput1, [i32; 4])> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                let desc = output.GetDesc()?;
                let r = desc.DesktopCoordinates;
                if desc.AttachedToDesktop.as_bool()
                    && (r.left..r.right).contains(&point.0)
                    && (r.top..r.bottom).contains(&point.1)
                {
                    return Ok((adapter, output.cast()?, [r.left, r.top, r.right, r.bottom]));
                }
                oi += 1;
            }
            ai += 1;
        }
    }
    Err("合成帧检查找不到检查点所在显示器".into())
}

fn staging(device: &ID3D11Device, width: u32, height: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
    Ok(texture.ok_or("合成帧检查暂存纹理为空")?)
}

fn read(
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
) -> Result<Vec<[u8; 4]>> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(texture, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for y in 0..height as usize {
        let line = unsafe {
            std::slice::from_raw_parts(
                (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                width as usize * 4,
            )
        };
        pixels.extend(line.chunks_exact(4).map(|p| [p[0], p[1], p[2], p[3]]));
    }
    unsafe { context.Unmap(texture, 0) };
    Ok(pixels)
}

/// 在后台线程运行直到 `stop`；`point` 由主任务按每档更新。
pub(super) fn record(point: Arc<AtomicI64>, stop: Arc<AtomicBool>) -> Recording {
    let mut recording = Recording {
        frames: Vec::new(),
        output: [0; 4],
        work: [0; 4],
        error: None,
    };
    if let Err(error) = record_into(&mut recording, &point, &stop) {
        recording.error = Some(error.to_string());
    }
    recording
}

fn record_into(recording: &mut Recording, point: &AtomicI64, stop: &AtomicBool) -> Result<()> {
    let (adapter, output, bounds) = output_at(unpack(point.load(Ordering::Acquire)))?;
    recording.output = bounds;
    recording.work = bounds;
    unsafe {
        use windows::Win32::Graphics::Gdi::*;
        let center = POINT {
            x: (bounds[0] + bounds[2]) / 2,
            y: (bounds[1] + bounds[3]) / 2,
        };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(
            MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST),
            &mut info,
        )
        .as_bool()
        {
            let r = info.rcWork;
            recording.work = [r.left, r.top, r.right, r.bottom];
        }
    }
    let (width, height) = (
        (bounds[2] - bounds[0]) as u32,
        (bounds[3] - bounds[1]) as u32,
    );
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device = device.ok_or("合成帧检查 D3D11 设备为空")?;
    let context = context.ok_or("合成帧检查 D3D11 上下文为空")?;
    let duplication = unsafe { output.DuplicateOutput(&device)? };
    let row_texture = staging(&device, width, 1)?;
    let column_texture = staging(&device, 1, height)?;
    while !stop.load(Ordering::Acquire) {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        match unsafe { duplication.AcquireNextFrame(50, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
            Err(error) => return Err(error.into()),
        }
        let at = qpc()?;
        // 每个成功的 Acquire 都要 Release；只有指针更新的帧不复制。
        let copied = (|| -> Result<Option<Frame>> {
            if info.LastPresentTime == 0 {
                return Ok(None);
            }
            let texture: ID3D11Texture2D = resource.ok_or("合成帧为空")?.cast()?;
            let at_point = unpack(point.load(Ordering::Acquire));
            let local = (at_point.0 - bounds[0], at_point.1 - bounds[1]);
            ensure(
                (0..width as i32).contains(&local.0) && (0..height as i32).contains(&local.1),
                "合成帧检查点移出显示器",
            )?;
            let row_box = D3D11_BOX {
                left: 0,
                top: local.1 as u32,
                front: 0,
                right: width,
                bottom: local.1 as u32 + 1,
                back: 1,
            };
            let column_box = D3D11_BOX {
                left: local.0 as u32,
                top: 0,
                front: 0,
                right: local.0 as u32 + 1,
                bottom: height,
                back: 1,
            };
            unsafe {
                context.CopySubresourceRegion(
                    &row_texture,
                    0,
                    0,
                    0,
                    0,
                    &texture,
                    0,
                    Some(&row_box),
                );
                context.CopySubresourceRegion(
                    &column_texture,
                    0,
                    0,
                    0,
                    0,
                    &texture,
                    0,
                    Some(&column_box),
                );
            }
            Ok(Some(Frame {
                qpc: at,
                accumulated: info.AccumulatedFrames,
                point: at_point,
                row: read(&context, &row_texture, width, 1)?,
                column: read(&context, &column_texture, 1, height)?,
            }))
        })();
        unsafe { duplication.ReleaseFrame()? };
        if let Some(frame) = copied? {
            recording.frames.push(frame);
        }
        if recording.frames.len() > 4000 {
            return Err("合成帧检查帧数超出上限".into());
        }
    }
    Ok(())
}

fn close(a: [u8; 4], b: [u8; 4], tolerance: i32) -> bool {
    (0..3).all(|i| (i32::from(a[i]) - i32::from(b[i])).abs() <= tolerance)
}
/// BGRA：红色明显占优即视为标记（HDR 下颜色会变亮，只看相对关系）。
fn bar_like(p: [u8; 4]) -> bool {
    i32::from(p[2]) > i32::from(p[1]) + 60 && i32::from(p[2]) > i32::from(p[0]) + 60
}

/// 一条扫描线上：从 `start` 向两侧走出贴图（底色或标记），返回外框 [起, 止) 与标记中心。
/// 走到显示器边界的一侧记为被裁。
struct Span {
    start: i32,
    end: i32,
    clipped: bool,
    bar: Option<f32>,
}
fn span(line: &[[u8; 4]], start: usize, background: [u8; 4]) -> Option<Span> {
    let inside = |p: [u8; 4]| close(p, background, 24) || bar_like(p);
    if !close(line[start], background, 24) {
        return None;
    }
    let mut lo = start;
    while lo > 0 && inside(line[lo - 1]) {
        lo -= 1;
    }
    let mut hi = start + 1;
    while hi < line.len() && inside(line[hi]) {
        hi += 1;
    }
    // 标记取外框内最长的一段红色连续像素的中心。
    let (mut best, mut run_start) = (None::<(usize, usize)>, None);
    for i in lo..=hi {
        let bar = i < hi && bar_like(line[i]);
        match (bar, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(s)) => {
                if best.is_none_or(|(a, b)| i - s > b - a) {
                    best = Some((s, i));
                }
                run_start = None;
            }
            _ => {}
        }
    }
    Some(Span {
        start: lo as i32,
        end: hi as i32,
        clipped: lo == 0 || hi == line.len(),
        bar: best.map(|(a, b)| (a + b) as f32 / 2.),
    })
}

pub(super) struct Measured {
    /// 屏幕物理坐标 [left, top, right, bottom]。
    pub rect: [i32; 4],
    /// 标记在外框内的比例（x 取竖条，y 取横条）。
    pub fraction: Option<(f32, f32)>,
}

pub(super) fn measure(frame: &Frame, output: [i32; 4], background: [u8; 4]) -> Option<Measured> {
    let local = (frame.point.0 - output[0], frame.point.1 - output[1]);
    let row = span(&frame.row, local.0 as usize, background)?;
    let column = span(&frame.column, local.1 as usize, background)?;
    let fraction = match (row.bar, column.bar, row.clipped || column.clipped) {
        (Some(x), Some(y), false) => Some((
            (x - row.start as f32) / (row.end - row.start) as f32,
            (y - column.start as f32) / (column.end - column.start) as f32,
        )),
        _ => None,
    };
    Some(Measured {
        rect: [
            output[0] + row.start,
            output[1] + column.start,
            output[0] + row.end,
            output[1] + column.end,
        ],
        fraction,
    })
}

pub(super) fn background(frame: &Frame, output: [i32; 4]) -> [u8; 4] {
    frame.row[(frame.point.0 - output[0]) as usize]
}

pub(super) struct Step {
    pub posted: i64,
    /// 屏幕物理坐标 [left, top, right, bottom]。
    pub before: [i32; 4],
    pub after: Option<[i32; 4]>,
}

/// 按档统计：贴图缺失、外框不属于缩放前后、标记比例与首帧不符（>3px）的合成帧数。
pub(super) fn analyze(
    recording: &Recording,
    steps: &[Step],
    stopped: i64,
    frequency: i64,
) -> serde_json::Value {
    let (output, work) = (recording.output, recording.work);
    let ms = |from: i64, to: i64| (to - from) as f64 * 1000. / frequency as f64;
    // 超出工作区的边：任务栏等置顶窗口可能在贴图上方或下方，测得边落在工作区与显示器边界之间即可。
    let matches = |measured: [i32; 4], rect: [i32; 4]| {
        (0..4).all(|i| {
            let (inner, outer) = (work[i], output[i]);
            let beyond = if i < 2 {
                rect[i] < inner
            } else {
                rect[i] > inner
            };
            if beyond {
                let (lo, hi) = if i < 2 {
                    (rect[i].max(outer), inner)
                } else {
                    (inner, rect[i].min(outer))
                };
                (lo - 2..=hi + 2).contains(&measured[i])
            } else {
                (measured[i] - rect[i]).abs() <= 2
            }
        })
    };
    let first = steps.first().and_then(|step| {
        recording
            .frames
            .iter()
            .filter(|frame| frame.qpc < step.posted)
            .last()
    });
    let Some(first) = first else {
        return serde_json::json!({"ok":false,"error":"没有缩放前的参考帧","recording_error":recording.error});
    };
    let background = background(first, output);
    let reference = measure(first, output, background);
    let Some(reference_fraction) = reference.as_ref().and_then(|m| m.fraction) else {
        return serde_json::json!({"ok":false,"error":"参考帧没有找到完整外框与标记","recording_error":recording.error});
    };
    let (mut missing, mut rect_bad, mut content_bad, mut skipped, mut total) = (0, 0, 0, 0, 0);
    let mut rows = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let until = steps.get(index + 1).map_or(stopped, |next| next.posted);
        let (mut step_missing, mut step_rect, mut step_content, mut step_frames) = (0, 0, 0, 0);
        let mut bad = Vec::new();
        let mut times = Vec::new();
        for frame in recording
            .frames
            .iter()
            .filter(|frame| frame.qpc > step.posted && frame.qpc < until)
        {
            step_frames += 1;
            skipped += frame.accumulated.saturating_sub(1);
            let at = ms(step.posted, frame.qpc);
            times.push((at * 10.).round() / 10.);
            let Some(m) = measure(frame, output, background) else {
                step_missing += 1;
                bad.push(serde_json::json!({"ms":at,"kind":"missing"}));
                continue;
            };
            if !(matches(m.rect, step.before)
                || step.after.is_some_and(|after| matches(m.rect, after)))
            {
                step_rect += 1;
                bad.push(serde_json::json!({"ms":at,"kind":"rect","rect":m.rect}));
                continue;
            }
            if let Some(fraction) = m.fraction {
                let dx = (fraction.0 - reference_fraction.0).abs() * (m.rect[2] - m.rect[0]) as f32;
                let dy = (fraction.1 - reference_fraction.1).abs() * (m.rect[3] - m.rect[1]) as f32;
                if dx.max(dy) > 3. {
                    step_content += 1;
                    bad.push(serde_json::json!({"ms":at,"kind":"content","rect":m.rect,"offset_px":[dx,dy]}));
                }
            }
        }
        total += step_frames;
        missing += step_missing;
        rect_bad += step_rect;
        content_bad += step_content;
        rows.push(serde_json::json!({
            "frames":step_frames,"missing":step_missing,"rect_mismatch":step_rect,
            "content_mismatch":step_content,"bad":bad,"frame_ms":times,
            "before":step.before,"after":step.after,
        }));
    }
    serde_json::json!({
        "ok":recording.error.is_none() && missing + rect_bad + content_bad == 0,
        "frames":total,"missing":missing,"rect_mismatch":rect_bad,"content_mismatch":content_bad,
        "skipped_frames":skipped,"output":output,"work":work,
        "reference":{"rect":reference.map(|m| m.rect),"fraction":[reference_fraction.0,reference_fraction.1]},
        "recording_error":recording.error,"steps":rows,
    })
}
