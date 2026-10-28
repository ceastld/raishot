#[path = "recording_capture_result.rs"]
pub(crate) mod cancellation;
#[path = "capture_frame_dxgi.rs"]
mod frame_dxgi;
#[path = "capture_frame_lease.rs"]
mod frame_lease;
#[path = "capture_hdr.rs"]
mod hdr;

use crate::{
    geometry::{Rect, Rotation, ScreenRect},
    Result,
};
use cancellation::CaptureCancelled;
use frame_dxgi::DxgiFrame;
use frame_lease::{FrameLease, LaneGate};
use std::{error::Error, fmt, mem::size_of, time::Instant};
use windows::{
    core::Interface,
    Win32::{
        Foundation::{HMODULE, POINT, RECT},
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
        System::{ProcessStatus::*, Threading::GetCurrentProcess},
        UI::WindowsAndMessaging::GetCursorPos,
    },
};

pub struct Snapshot {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    texture: Option<ID3D11Texture2D>,
    /// 纹理实际存储的完整 raw 屏幕坐标范围；桌面/旋转元数据仍描述整块输出。
    texture_storage: Rect,
    pub desktop: RECT,
    pub rotation: Rotation,
    pub raw_width: u32,
    pub raw_height: u32,
    pub width: u32,
    pub height: u32,
    pub capture_ms: f64,
    pub adapter_name: String,
    adapter: IDXGIAdapter1,
}

pub struct Image {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub readback_ms: f64,
}

const MAX_CAPTURE_TEXTURE_SIDE: u32 = 16_384;
const MAX_CROPPED_CAPTURE_PIXELS: u64 = 32_000_000;

fn capture_texture_storage(
    raw_size: (u32, u32),
    rotation: Rotation,
    roi: Option<Rect>,
) -> Result<Rect> {
    if raw_size.0 == 0
        || raw_size.1 == 0
        || raw_size.0 > MAX_CAPTURE_TEXTURE_SIDE
        || raw_size.1 > MAX_CAPTURE_TEXTURE_SIDE
    {
        return Err("显示器原始纹理尺寸无效".into());
    }
    let full = Rect::full(raw_size.0, raw_size.1);
    let Some(roi) = roi else { return Ok(full) };
    let (raw, _) = capture_texture_read_region(raw_size, rotation, full, roi)?;
    if u64::from(raw.width) * u64::from(raw.height) > MAX_CROPPED_CAPTURE_PIXELS {
        return Err("采集纹理选区超过 3200 万像素".into());
    }
    Ok(raw)
}

/// 返回原始 raw 范围及其在实际存储纹理内的局部范围。
fn capture_texture_read_region(
    raw_size: (u32, u32),
    rotation: Rotation,
    storage: Rect,
    logical: Rect,
) -> Result<(Rect, Rect)> {
    let (width, height) = rotation.size(raw_size.0, raw_size.1);
    if !logical.valid(width, height) || !storage.valid(raw_size.0, raw_size.1) {
        return Err("采集选区或存储纹理范围无效".into());
    }
    let raw = rotation.raw_rect(logical, raw_size.0, raw_size.1);
    if raw.x < storage.x
        || raw.y < storage.y
        || u64::from(raw.x) + u64::from(raw.width) > u64::from(storage.x) + u64::from(storage.width)
        || u64::from(raw.y) + u64::from(raw.height)
            > u64::from(storage.y) + u64::from(storage.height)
    {
        return Err("采集选区超出已保存的纹理范围".into());
    }
    Ok((
        raw,
        Rect {
            x: raw.x - storage.x,
            y: raw.y - storage.y,
            ..raw
        },
    ))
}

fn capture_texture_refresh_region(
    raw_size: (u32, u32),
    rotation: Rotation,
    storage: Rect,
    roi: Option<Rect>,
) -> Result<Option<(Rect, Rect)>> {
    if !storage.valid(raw_size.0, raw_size.1) {
        return Err("采集存储纹理范围无效".into());
    }
    if let Some(roi) = roi {
        capture_texture_read_region(raw_size, rotation, storage, roi).map(Some)
    } else if storage == Rect::full(raw_size.0, raw_size.1) {
        Ok(None)
    } else {
        Err("裁剪采集纹理不能刷新完整显示器，请使用初始化选区".into())
    }
}

#[cfg(test)]
#[path = "capture_texture_tests.rs"]
mod texture_tests;

/// 将已裁剪的未旋转 staging 行转为紧凑 BGRA，不把显卡行间 padding 带入图片。
fn copy_identity_bgra_rows(
    source: &[u8],
    row_pitch: usize,
    target: &mut [u8],
    row_bytes: usize,
) -> Result<()> {
    if row_bytes == 0
        || !row_bytes.is_multiple_of(4)
        || row_pitch < row_bytes
        || target.is_empty()
        || !target.len().is_multiple_of(row_bytes)
    {
        return Err("桌面读回行尺寸无效".into());
    }
    let rows = target.len() / row_bytes;
    let needed = row_pitch
        .checked_mul(rows - 1)
        .and_then(|bytes| bytes.checked_add(row_bytes))
        .ok_or("桌面读回行尺寸溢出")?;
    if source.len() < needed {
        return Err("桌面读回缓冲区不足".into());
    }
    for (y, row) in target.chunks_exact_mut(row_bytes).enumerate() {
        let start = y * row_pitch;
        row.copy_from_slice(&source[start..start + row_bytes]);
        // DXGI 桌面 alpha 不可靠；保持原读回路径的不透明语义。
        for pixel in row.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }
    Ok(())
}

fn targets() -> Result<Vec<(IDXGIAdapter1, IDXGIOutput, DXGI_OUTPUT_DESC)>> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut targets = Vec::new();
        let mut ai = 0;
        loop {
            let adapter = match factory.EnumAdapters1(ai) {
                Ok(a) => a,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            let mut oi = 0;
            loop {
                let output = match adapter.EnumOutputs(oi) {
                    Ok(o) => o,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(e.into()),
                };
                let desc = output.GetDesc()?;
                let r = desc.DesktopCoordinates;
                if desc.AttachedToDesktop.as_bool()
                    && r.right > r.left
                    && r.bottom > r.top
                    && !targets.iter().any(
                        |(_, _, d): &(IDXGIAdapter1, IDXGIOutput, DXGI_OUTPUT_DESC)| {
                            d.DesktopCoordinates == r
                        },
                    )
                {
                    targets.push((adapter.clone(), output, desc));
                }
                oi += 1;
            }
            ai += 1;
        }
        if targets.is_empty() {
            return Err("未找到活动显示器".into());
        }
        Ok(targets)
    }
}

fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    create_device_with_flags(adapter, D3D11_CREATE_DEVICE_BGRA_SUPPORT)
}

fn create_device_with_flags(
    adapter: &IDXGIAdapter1,
    flags: D3D11_CREATE_DEVICE_FLAG,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        Ok((
            device.ok_or("D3D11 未返回设备")?,
            context.ok_or("D3D11 未返回上下文")?,
        ))
    }
}

/// 只有没有持有已成功 Acquire 帧的初始化分支可构造此错误。
/// 租约排空、Release、冻结纹理读回等错误绝不能包装成它。
#[derive(Debug)]
struct InitialCaptureError {
    operation: &'static str,
    detail: String,
    source: windows::core::Error,
}

impl fmt::Display for InitialCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}：{}", self.detail, self.source)
    }
}

impl Error for InitialCaptureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

fn initial_capture_error(
    operation: &'static str,
    detail: String,
    source: windows::core::Error,
) -> Box<dyn Error> {
    Box::new(InitialCaptureError {
        operation,
        detail,
        source,
    })
}

/// 显式刷新只在尚未取得有效桌面帧、DXGI 不可用时转用 GDI。
/// 不递归追踪任意 source，避免把租约收尾或读回里的 HRESULT 当成兼容性失败。
pub(crate) fn can_fallback_refresh(error: &(dyn Error + 'static)) -> bool {
    if let Some(initial) = error.downcast_ref::<InitialCaptureError>() {
        return matches!(
            initial.operation,
            "D3D11CreateDevice" | "DuplicateOutput" | "CreateQuery" | "AcquireNextFrame"
        ) && [
            DXGI_ERROR_UNSUPPORTED,
            DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
            DXGI_ERROR_SESSION_DISCONNECTED,
            windows::Win32::Foundation::E_ACCESSDENIED,
            windows::Win32::Foundation::E_NOTIMPL,
        ]
        .contains(&initial.source.code());
    }
    error
        .downcast_ref::<InitialRecoveryFailure>()
        .is_some_and(|failure| {
            can_fallback_refresh(failure.first.as_ref())
                && can_fallback_refresh(failure.retry.as_ref())
        })
}

fn can_rebuild_initial(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<InitialCaptureError>()
        .is_some_and(|initial| {
            matches!(
                initial.operation,
                "D3D11CreateDevice" | "DuplicateOutput" | "CreateQuery" | "AcquireNextFrame"
            ) && [
                DXGI_ERROR_ACCESS_LOST,
                DXGI_ERROR_DEVICE_REMOVED,
                DXGI_ERROR_DEVICE_RESET,
                DXGI_ERROR_DRIVER_INTERNAL_ERROR,
            ]
            .contains(&initial.source.code())
        })
}

#[derive(Debug)]
struct InitialRecoveryFailure {
    first: Box<dyn Error>,
    retry: Box<dyn Error>,
}

impl fmt::Display for InitialRecoveryFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "初始桌面采集失效：{}；仅一次重建后仍失败：{}",
            self.first, self.retry
        )
    }
}

impl Error for InitialRecoveryFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.retry.as_ref())
    }
}

/// 此调用尚未交出冻结桌面；失败的整批局部资源先析构，再重枚举一次。
/// 不循环等待桌面、不读取新请求，也不允许成功结果或已持有帧的错误重截。
fn capture_initial_with_recovery<T>(
    mut capture: impl FnMut() -> Result<T>,
    cancelled: &dyn Fn() -> bool,
) -> Result<T> {
    if cancelled() {
        return Err(CaptureCancelled.into());
    }
    let first = match capture() {
        Ok(value) => {
            if cancelled() {
                return Err(CaptureCancelled.into());
            }
            return Ok(value);
        }
        Err(error) => error,
    };
    // 真实失败与取消同遇仍保留失败，但不开始旧请求的重建。
    if !can_rebuild_initial(first.as_ref()) || cancelled() {
        return Err(first);
    }
    match capture() {
        Ok(value) if !cancelled() => Ok(value),
        Ok(_) => Err(CaptureCancelled.into()),
        Err(retry) => Err(Box::new(InitialRecoveryFailure { first, retry })),
    }
}

impl Snapshot {
    pub fn capture() -> Result<Self> {
        unsafe {
            let mut cursor = POINT::default();
            GetCursorPos(&mut cursor)?;
            let target = targets()?
                .into_iter()
                .find(|(_, _, desc)| {
                    let r = desc.DesktopCoordinates;
                    cursor.x >= r.left
                        && cursor.x < r.right
                        && cursor.y >= r.top
                        && cursor.y < r.bottom
                })
                .ok_or("未找到鼠标所在的活动显示器")?;
            let (device, context) = create_device(&target.0)?;
            Self::capture_output(target, device, context)
        }
    }

    pub fn capture_all() -> Result<Vec<Self>> {
        Self::capture_all_once(None)
    }

    /// 仅短截图首显之前允许一次整批设备/复制器重建；录屏和长截图不调用它。
    pub(crate) fn capture_all_initial(cancelled: &dyn Fn() -> bool) -> Result<Vec<Self>> {
        capture_initial_with_recovery(|| Self::capture_all_once(Some(cancelled)), cancelled)
    }

    fn capture_all_once(cancelled: Option<&dyn Fn() -> bool>) -> Result<Vec<Self>> {
        unsafe {
            // 同一适配器共享捕获设备；跨显卡各自捕获，避免假定全部屏幕属于主显卡。
            let mut devices = std::collections::HashMap::new();
            let mut snapshots = Vec::new();
            for target in targets()? {
                if cancelled.is_some_and(|cancelled| cancelled()) {
                    return Err(CaptureCancelled.into());
                }
                let luid = target.0.GetDesc1()?.AdapterLuid;
                let key = (luid.HighPart, luid.LowPart);
                let (device, context) = if let Some(pair) = devices.get(&key) {
                    let pair: &(ID3D11Device, ID3D11DeviceContext) = pair;
                    pair.clone()
                } else {
                    let pair = create_device(&target.0).map_err(|error| {
                        // 旧 capture_all 的错误合同保留；只有可取消首显入口标记阶段。
                        if cancelled.is_none() {
                            return error;
                        }
                        match error.downcast::<windows::core::Error>() {
                            Ok(source) => initial_capture_error(
                                "D3D11CreateDevice",
                                "无法初始化桌面采集设备".into(),
                                *source,
                            ),
                            Err(error) => error,
                        }
                    })?;
                    devices.insert(key, pair.clone());
                    pair
                };
                if cancelled.is_some_and(|cancelled| cancelled()) {
                    return Err(CaptureCancelled.into());
                }
                snapshots.push(Self::capture_output_inner(
                    target, device, context, None, cancelled,
                )?);
            }
            Ok(snapshots)
        }
    }

    /// 长截图只初始化与物理选区相交的输出；不为无关显示器创建纹理/桌面复制。
    pub(crate) fn capture_region_cancellable(
        region: ScreenRect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Self>> {
        Self::capture_region_with_video(region, cancelled, false)
    }

    /// 仅录屏请求视频处理设备，截图/长截图的设备合同保持原样。
    pub(crate) fn capture_recording_region(
        region: ScreenRect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Self>> {
        Self::capture_region_with_video(region, cancelled, true)
    }

    fn capture_region_with_video(
        region: ScreenRect,
        cancelled: &dyn Fn() -> bool,
        video: bool,
    ) -> Result<Vec<Self>> {
        if region.width == 0
            || region.height == 0
            || region.left as i64 + region.width as i64 > i32::MAX as i64
            || region.top as i64 + region.height as i64 > i32::MAX as i64
        {
            return Err("采集物理范围无效".into());
        }
        let right = region.left as i64 + region.width as i64;
        let bottom = region.top as i64 + region.height as i64;
        let mut devices = std::collections::HashMap::new();
        let mut snapshots = Vec::new();
        for target in targets()? {
            if cancelled() {
                return Err(CaptureCancelled.into());
            }
            let desktop = target.2.DesktopCoordinates;
            let left = (desktop.left as i64).max(region.left as i64);
            let top = (desktop.top as i64).max(region.top as i64);
            let part_right = (desktop.right as i64).min(right);
            let part_bottom = (desktop.bottom as i64).min(bottom);
            if left >= part_right || top >= part_bottom {
                continue;
            }
            let rect = Rect {
                x: (left - desktop.left as i64) as u32,
                y: (top - desktop.top as i64) as u32,
                width: (part_right - left) as u32,
                height: (part_bottom - top) as u32,
            };
            unsafe {
                let luid = target.0.GetDesc1()?.AdapterLuid;
                let key = (luid.HighPart, luid.LowPart);
                let (device, context) = if let Some(pair) = devices.get(&key) {
                    let pair: &(ID3D11Device, ID3D11DeviceContext) = pair;
                    pair.clone()
                } else {
                    // D3D11CreateDevice 是不可中断的驱动调用，返回后立即重查取消。
                    let pair = if video {
                        create_device_with_flags(
                            &target.0,
                            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                        )
                        .or_else(|_| create_device(&target.0))?
                    } else {
                        create_device(&target.0)?
                    };
                    devices.insert(key, pair.clone());
                    pair
                };
                if cancelled() {
                    return Err(CaptureCancelled.into());
                }
                snapshots.push(Self::capture_output_inner(
                    target,
                    device,
                    context,
                    Some(rect),
                    Some(cancelled),
                )?);
            }
        }
        if snapshots.is_empty() {
            return Err("采集选区未与活动显示器相交".into());
        }
        Ok(snapshots)
    }

    fn capture_output(
        target: (IDXGIAdapter1, IDXGIOutput, DXGI_OUTPUT_DESC),
        device: ID3D11Device,
        context: ID3D11DeviceContext,
    ) -> Result<Self> {
        Self::capture_output_inner(target, device, context, None, None)
    }

    fn capture_output_inner(
        target: (IDXGIAdapter1, IDXGIOutput, DXGI_OUTPUT_DESC),
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        roi: Option<Rect>,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Result<Self> {
        let start = Instant::now();
        let (adapter, output, desc) = target;
        unsafe {
            let output: IDXGIOutput1 = output.cast()?;
            let (duplication, sdr) = hdr::duplicate(&output, &device, true).map_err(|error| {
                initial_capture_error(
                    "DuplicateOutput",
                    format!(
                        "无法取得桌面 GPU 图像（初始复制器，屏幕原点 {},{}，{}），请检查桌面会话、显卡驱动或同进程重复采集",
                        desc.DesktopCoordinates.left,
                        desc.DesktopCoordinates.top,
                        if roi.is_some() { "选区模式" } else { "全屏模式" },
                    ),
                    error,
                )
            })?;
            // 先创建同步对象，避免创建失败时已有拷贝却无法等待归还帧。
            let mut query = None;
            device
                .CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: D3D11_QUERY_EVENT,
                        MiscFlags: 0,
                    },
                    Some(&mut query),
                )
                .map_err(|error| {
                    initial_capture_error("CreateQuery", "无法创建 GPU 同步查询".into(), error)
                })?;
            let query = query.ok_or("无法创建 GPU 同步查询")?;
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            let wait = Instant::now();
            loop {
                if cancelled.is_some_and(|cancelled| cancelled()) {
                    return Err(CaptureCancelled.into());
                }
                match duplication.AcquireNextFrame(
                    if cancelled.is_some() { 20 } else { 100 },
                    &mut info,
                    &mut resource,
                ) {
                    Ok(()) => {
                        // 指针更新也会唤醒 AcquireNextFrame，此时桌面纹理可能尚未填充。
                        if info.LastPresentTime == 0 && info.AccumulatedFrames == 0 {
                            resource = None;
                            FrameLease::new(DxgiFrame::new(
                                &duplication,
                                &device,
                                &context,
                                &query,
                            ))
                            .finish(Ok(()), cancelled)?;
                            if wait.elapsed().as_secs_f64() > 2.0 {
                                return Err(
                                    "未收到有效桌面图像；请唤醒显示器或检查当前桌面会话".into()
                                );
                            }
                            continue;
                        }
                        break;
                    }
                    Err(e)
                        if e.code() == DXGI_ERROR_WAIT_TIMEOUT
                            && wait.elapsed().as_secs_f64() < 2.0 =>
                    {
                        continue
                    }
                    // Acquire 失败没有取得帧；成功后的任何出口仍只经过 FrameLease。
                    Err(error) => {
                        return Err(initial_capture_error(
                            "AcquireNextFrame",
                            "等待桌面截图失败".into(),
                            error,
                        ));
                    }
                }
            }
            let mut lease =
                FrameLease::new(DxgiFrame::new(&duplication, &device, &context, &query));
            let frame = (|| -> Result<Self> {
                if cancelled.is_some_and(|cancelled| cancelled()) {
                    return Err(CaptureCancelled.into());
                }
                if std::env::var_os("RUSTSHOT_TRACE").is_some() {
                    eprintln!(
                        "DXGI frame: present={} accumulated={} pointer={}",
                        info.LastPresentTime, info.AccumulatedFrames, info.LastMouseUpdateTime
                    );
                }
                let acquired: ID3D11Texture2D = resource.ok_or("桌面帧为空")?.cast()?;
                let mut td = D3D11_TEXTURE2D_DESC::default();
                acquired.GetDesc(&mut td);
                // HDR 帧换算后仍存成 BGRA8，自有纹理因此需要能作渲染目标。
                let sdr = match td.Format {
                    DXGI_FORMAT_B8G8R8A8_UNORM => None,
                    DXGI_FORMAT_R16G16B16A16_FLOAT if sdr.is_some() => sdr.as_ref(),
                    _ => return Err("桌面帧格式不受支持（仅 BGRA8 或 HDR FP16）".into()),
                };
                let raw_size = (td.Width, td.Height);
                let rotation = output_rotation(desc.Rotation);
                let storage = capture_texture_storage(raw_size, rotation, roi)?;
                td.Width = storage.width;
                td.Height = storage.height;
                td.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
                td.Usage = D3D11_USAGE_DEFAULT;
                td.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32
                    | if sdr.is_some() {
                        D3D11_BIND_RENDER_TARGET.0 as u32
                    } else {
                        0
                    };
                td.CPUAccessFlags = 0;
                td.MiscFlags = 0;
                let mut texture = None;
                device.CreateTexture2D(&td, None, Some(&mut texture))?;
                let texture = texture.ok_or("无法创建桌面纹理")?;
                let pass = sdr
                    .map(|sdr| {
                        sdr.prepare(
                            &device,
                            &acquired,
                            storage,
                            &texture,
                            Rect::full(storage.width, storage.height),
                        )
                    })
                    .transpose()?;

                if cancelled.is_some_and(|cancelled| cancelled()) {
                    return Err(CaptureCancelled.into());
                }
                lease.backend_mut().retain_copy(&acquired, &texture);
                lease.submit_copy(|| {
                    if let Some(pass) = &pass {
                        pass.draw(&context, &acquired);
                    } else if roi.is_some() {
                        let source_box = D3D11_BOX {
                            left: storage.x,
                            top: storage.y,
                            front: 0,
                            right: storage.x + storage.width,
                            bottom: storage.y + storage.height,
                            back: 1,
                        };
                        context.CopySubresourceRegion(
                            &texture,
                            0,
                            0,
                            0,
                            0,
                            &acquired,
                            0,
                            Some(&source_box),
                        );
                    } else {
                        context.CopyResource(&texture, &acquired);
                    }
                })?;
                let (width, height) = rotation.size(raw_size.0, raw_size.1);
                if width != (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32
                    || height
                        != (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32
                {
                    return Err("显示器模式已变化，请重新截图".into());
                }
                let ad = adapter.GetDesc1()?;
                let name_end = ad
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(ad.Description.len());
                Ok(Self {
                    device,
                    context,
                    texture: Some(texture),
                    texture_storage: storage,
                    desktop: desc.DesktopCoordinates,
                    rotation,
                    raw_width: raw_size.0,
                    raw_height: raw_size.1,
                    width,
                    height,
                    capture_ms: start.elapsed().as_secs_f64() * 1000.0,
                    adapter_name: String::from_utf16_lossy(&ad.Description[..name_end]),
                    adapter,
                })
            })();
            let mut snapshot = lease.finish(frame, cancelled)?;
            // 保留完整初始采集测量，不能漏记 GPU 排空和归还阶段。
            snapshot.capture_ms = start.elapsed().as_secs_f64() * 1000.0;
            Ok(snapshot)
        }
    }

    pub fn read_region(&self, rect: Rect) -> Result<Image> {
        self.read_region_inner(rect, None)
    }

    pub(crate) fn read_region_cancellable(
        &self,
        rect: Rect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Image> {
        self.read_region_inner(rect, Some(cancelled))
    }

    fn read_region_inner(&self, rect: Rect, cancelled: Option<&dyn Fn() -> bool>) -> Result<Image> {
        if !rect.valid(self.width, self.height) {
            return Err("选区为空或超出显示器边界".into());
        }
        if cancelled.is_some_and(|cancelled| cancelled()) {
            return Err(CaptureCancelled.into());
        }
        unsafe {
            let start = Instant::now();
            let (raw, texture_local) = capture_texture_read_region(
                (self.raw_width, self.raw_height),
                self.rotation,
                self.texture_storage_rect(),
                rect,
            )?;
            let td = D3D11_TEXTURE2D_DESC {
                Width: raw.width,
                Height: raw.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut staging = None;
            self.device.CreateTexture2D(&td, None, Some(&mut staging))?;
            let staging = staging.ok_or("无法创建选区读回纹理")?;
            let source_box = D3D11_BOX {
                left: texture_local.x,
                top: texture_local.y,
                front: 0,
                right: texture_local.x + texture_local.width,
                bottom: texture_local.y + texture_local.height,
                back: 1,
            };
            if cancelled.is_some_and(|cancelled| cancelled()) {
                return Err(CaptureCancelled.into());
            }
            self.context.CopySubresourceRegion(
                &staging,
                0,
                0,
                0,
                0,
                self.texture(),
                0,
                Some(&source_box),
            );
            let bytes = (rect.width as usize)
                .checked_mul(rect.height as usize)
                .and_then(|n| n.checked_mul(4))
                .ok_or("选区过大")?;
            let mut bgra = Vec::new();
            bgra.try_reserve_exact(bytes)
                .map_err(|_| "选区读回内存不足")?;
            bgra.resize(bytes, 0);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if let Some(cancelled) = cancelled {
                // 非阻塞 Map 不依赖驱动隐式提交；这笔选区 staging 拷贝必须先入 GPU 队列。
                self.context.Flush();
                let wait = Instant::now();
                loop {
                    if cancelled() {
                        return Err(CaptureCancelled.into());
                    }
                    match self.context.Map(
                        &staging,
                        0,
                        D3D11_MAP_READ,
                        D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                        Some(&mut mapped),
                    ) {
                        Ok(()) => break,
                        Err(error)
                            if error.code() == DXGI_ERROR_WAS_STILL_DRAWING
                                && wait.elapsed().as_secs_f64() < 2. =>
                        {
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            } else {
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            }
            // 显卡行间有 padding，旋转显示器也只读回选区对应的小矩形。
            let copied = (|| -> Result<()> {
                if mapped.pData.is_null() || (mapped.RowPitch as usize) < raw.width as usize * 4 {
                    return Err("桌面读回行尺寸无效".into());
                }
                if matches!(self.rotation, Rotation::Identity) {
                    let row_bytes = rect.width as usize * 4;
                    let row_pitch = mapped.RowPitch as usize;
                    if mapped.pData.is_null() || row_pitch < row_bytes {
                        return Err("桌面读回行尺寸无效".into());
                    }
                    let mapped_bytes = row_pitch
                        .checked_mul(rect.height as usize - 1)
                        .and_then(|bytes| bytes.checked_add(row_bytes))
                        .ok_or("桌面读回行尺寸溢出")?;
                    // staging 已在 CopySubresourceRegion 中裁剪到选区，原点为 (0, 0)。
                    let source =
                        std::slice::from_raw_parts(mapped.pData.cast::<u8>(), mapped_bytes);
                    if let Some(cancelled) = cancelled {
                        for (index, block) in bgra.chunks_mut(row_bytes * 64).enumerate() {
                            if cancelled() {
                                return Err(CaptureCancelled.into());
                            }
                            copy_identity_bgra_rows(
                                &source[index * 64 * row_pitch..],
                                row_pitch,
                                block,
                                row_bytes,
                            )?;
                        }
                    } else {
                        copy_identity_bgra_rows(source, row_pitch, &mut bgra, row_bytes)?;
                    }
                } else {
                    for y in 0..rect.height {
                        if y.is_multiple_of(64) && cancelled.is_some_and(|cancelled| cancelled()) {
                            return Err(CaptureCancelled.into());
                        }
                        for x in 0..rect.width {
                            let (rx, ry) = self.rotation.raw_pixel(
                                rect.x + x,
                                rect.y + y,
                                self.raw_width,
                                self.raw_height,
                            );
                            let src = mapped.pData.cast::<u8>().add(
                                (ry - raw.y) as usize * mapped.RowPitch as usize
                                    + (rx - raw.x) as usize * 4,
                            );
                            let dst = ((y * rect.width + x) * 4) as usize;
                            std::ptr::copy_nonoverlapping(src, bgra.as_mut_ptr().add(dst), 4);
                            bgra[dst + 3] = 255;
                        }
                    }
                }
                Ok(())
            })();
            self.context.Unmap(&staging, 0);
            copied?;
            Ok(Image {
                width: rect.width,
                height: rect.height,
                bgra,
                readback_ms: start.elapsed().as_secs_f64() * 1000.0,
            })
        }
    }

    pub fn gpu_bytes(&self) -> Option<(u64, u64)> {
        unsafe {
            let adapter: IDXGIAdapter3 = self.adapter.cast().ok()?;
            let mut local = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            let mut nonlocal = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut local)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut nonlocal)
                .ok()?;
            Some((local.CurrentUsage, nonlocal.CurrentUsage))
        }
    }

    pub fn texture(&self) -> &ID3D11Texture2D {
        self.texture.as_ref().expect("活动截图纹理")
    }
    pub(crate) fn texture_storage_rect(&self) -> Rect {
        self.texture_storage
    }
}

fn output_rotation(rotation: DXGI_MODE_ROTATION) -> Rotation {
    match rotation {
        DXGI_MODE_ROTATION_ROTATE90 => Rotation::R90,
        DXGI_MODE_ROTATION_ROTATE180 => Rotation::R180,
        DXGI_MODE_ROTATION_ROTATE270 => Rotation::R270,
        _ => Rotation::Identity,
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        unsafe {
            self.context.ClearState();
            self.texture.take();
            // D3D11 允许延迟销毁；释放最后一个纹理引用后提交待处理销毁。
            self.context.Flush();
        }
    }
}

pub fn memory_mib() -> (f64, f64) {
    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        let ok = K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            counters.cb,
        );
        if !ok.as_bool() {
            return (f64::NAN, f64::NAN);
        }
        (
            counters.WorkingSetSize as f64 / 1048576.0,
            counters.PrivateUsage as f64 / 1048576.0,
        )
    }
}

// Snapshot 与新 duplication 分别建立；相同 raw 尺寸仍可能已转向 180°。
fn live_geometry_matches(
    desktop: RECT,
    rotation: Rotation,
    raw_size: (u32, u32),
    output: &DXGI_OUTPUT_DESC,
    duplication: &DXGI_OUTDUPL_DESC,
) -> bool {
    let expected_rotation = match rotation {
        Rotation::Identity => DXGI_MODE_ROTATION_IDENTITY,
        Rotation::R90 => DXGI_MODE_ROTATION_ROTATE90,
        Rotation::R180 => DXGI_MODE_ROTATION_ROTATE180,
        Rotation::R270 => DXGI_MODE_ROTATION_ROTATE270,
    };
    let canonical_rotation = |native: DXGI_MODE_ROTATION| match native {
        DXGI_MODE_ROTATION_UNSPECIFIED | DXGI_MODE_ROTATION_IDENTITY => {
            Some(DXGI_MODE_ROTATION_IDENTITY)
        }
        DXGI_MODE_ROTATION_ROTATE90
        | DXGI_MODE_ROTATION_ROTATE180
        | DXGI_MODE_ROTATION_ROTATE270 => Some(native),
        _ => None,
    };
    let logical_size = rotation.size(raw_size.0, raw_size.1);
    raw_size.0 != 0
        && raw_size.1 != 0
        && output.DesktopCoordinates == desktop
        // 未指定沿基线视作无旋转；未认识的枚举不能由旧转换器补成 Identity。
        && canonical_rotation(output.Rotation) == Some(expected_rotation)
        && canonical_rotation(duplication.Rotation) == Some(expected_rotation)
        // 这里只核 Snapshot 实测 raw 尺寸的逻辑自洽；新帧尺寸仍由 refresh 检查。
        // 不把 ModeDesc 的旋转维度推断成 acquired 纹理的 raw 维度。
        && i64::from(desktop.right) - i64::from(desktop.left) == i64::from(logical_size.0)
        && i64::from(desktop.bottom) - i64::from(desktop.top) == i64::from(logical_size.1)
}

// 连续采集只用于录屏/滚动采集，冻结截图仍在首帧后释放 Duplication。
pub struct LiveScreen {
    pub snapshot: Snapshot,
    duplication: IDXGIOutputDuplication,
    sdr: Option<hdr::SdrConverter>,
    query: ID3D11Query,
    lane: LaneGate,
}
impl LiveScreen {
    pub fn new(snapshot: Snapshot) -> Result<Self> {
        unsafe {
            let mut index = 0;
            let output = loop {
                let output = snapshot.adapter.EnumOutputs(index)?;
                if output.GetDesc()?.DesktopCoordinates == snapshot.desktop {
                    break output;
                }
                index += 1;
            };
            let output: IDXGIOutput1 = output.cast()?;
            // 首帧按 HDR 换算过的纹理才能继续接 FP16 帧，否则两者保持同一路径。
            let mut texture_desc = D3D11_TEXTURE2D_DESC::default();
            snapshot.texture().GetDesc(&mut texture_desc);
            let renderable = texture_desc.BindFlags & D3D11_BIND_RENDER_TARGET.0 as u32 != 0;
            let (duplication, sdr) = hdr::duplicate(&output, &snapshot.device, renderable)
                .map_err(|e| {
                    format!(
                        "无法创建连续桌面复制器（屏幕原点 {},{}）；请检查桌面会话或同进程重复采集：{e}",
                        snapshot.desktop.left, snapshot.desktop.top,
                    )
                })?;
            let duplication_desc = duplication.GetDesc();
            let output_desc = output.GetDesc()?;
            if !live_geometry_matches(
                snapshot.desktop,
                snapshot.rotation,
                (snapshot.raw_width, snapshot.raw_height),
                &output_desc,
                &duplication_desc,
            ) {
                return Err(tr!(
                    "无法确认连续采集器与首帧的屏幕位置、方向一致，请重新开始采集",
                    "Cannot confirm that continuous capture matches the initial display geometry; restart capture"
                )
                .into());
            }
            let mut query = None;
            snapshot.device.CreateQuery(
                &D3D11_QUERY_DESC {
                    Query: D3D11_QUERY_EVENT,
                    MiscFlags: 0,
                },
                Some(&mut query),
            )?;
            Ok(Self {
                snapshot,
                duplication,
                sdr,
                query: query.ok_or("无法创建连续采集同步查询")?,
                lane: LaneGate::default(),
            })
        }
    }
    pub fn refresh(&self) -> Result<()> {
        self.refresh_inner(None, None)
    }

    pub(crate) fn refresh_region_cancellable(
        &self,
        rect: Rect,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<()> {
        if !rect.valid(self.snapshot.width, self.snapshot.height) {
            return Err("连续采集选区超出显示器".into());
        }
        self.refresh_inner(Some(rect), Some(cancelled))
    }

    fn refresh_inner(&self, roi: Option<Rect>, cancelled: Option<&dyn Fn() -> bool>) -> Result<()> {
        // 失败 lane 在任何新 Acquire 前拒绝；纯取消不熔断健康的暂停/代次路径。
        self.lane.check()?;
        let copy_region = capture_texture_refresh_region(
            (self.snapshot.raw_width, self.snapshot.raw_height),
            self.snapshot.rotation,
            self.snapshot.texture_storage_rect(),
            roi,
        )?;
        let result = (|| -> Result<()> {
            unsafe {
                if cancelled.is_some_and(|cancelled| cancelled()) {
                    return Err(CaptureCancelled.into());
                }
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource = None;
                match self
                    .duplication
                    .AcquireNextFrame(0, &mut info, &mut resource)
                {
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(()),
                    Err(e) => return Err(e.into()),
                    Ok(()) => {}
                }
                let mut lease = FrameLease::new(DxgiFrame::new(
                    &self.duplication,
                    &self.snapshot.device,
                    &self.snapshot.context,
                    &self.query,
                ));
                let frame = (|| -> Result<()> {
                    if cancelled.is_some_and(|cancelled| cancelled()) {
                        return Err(CaptureCancelled.into());
                    }
                    if info.LastPresentTime == 0 {
                        return Ok(());
                    }
                    let acquired: ID3D11Texture2D = resource.ok_or("连续采集帧为空")?.cast()?;
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    acquired.GetDesc(&mut desc);
                    let sdr = match desc.Format {
                        DXGI_FORMAT_B8G8R8A8_UNORM => None,
                        DXGI_FORMAT_R16G16B16A16_FLOAT if self.sdr.is_some() => self.sdr.as_ref(),
                        _ => return Err("显示器模式变化，采集已停止".into()),
                    };
                    if desc.Width != self.snapshot.raw_width
                        || desc.Height != self.snapshot.raw_height
                    {
                        return Err("显示器模式变化，采集已停止".into());
                    }
                    let texture = self.snapshot.texture.as_ref().ok_or("连续采集纹理丢失")?;
                    let pass = sdr
                        .map(|sdr| {
                            let (source, target) = copy_region.unwrap_or_else(|| {
                                let full = Rect::full(desc.Width, desc.Height);
                                (full, full)
                            });
                            sdr.prepare(&self.snapshot.device, &acquired, source, texture, target)
                        })
                        .transpose()?;
                    if cancelled.is_some_and(|cancelled| cancelled()) {
                        return Err(CaptureCancelled.into());
                    }
                    lease.backend_mut().retain_copy(&acquired, texture);
                    lease.submit_copy(|| {
                        if let Some(pass) = &pass {
                            pass.draw(&self.snapshot.context, &acquired);
                        } else if let Some((raw, texture_local)) = copy_region {
                            let source_box = D3D11_BOX {
                                left: raw.x,
                                top: raw.y,
                                front: 0,
                                right: raw.x + raw.width,
                                bottom: raw.y + raw.height,
                                back: 1,
                            };
                            self.snapshot.context.CopySubresourceRegion(
                                texture,
                                0,
                                texture_local.x,
                                texture_local.y,
                                0,
                                &acquired,
                                0,
                                Some(&source_box),
                            );
                        } else {
                            self.snapshot.context.CopyResource(texture, &acquired);
                        }
                    })?;
                    Ok(())
                })();
                // 明确收尾后才能返回自有纹理；Release 错误不会由取消或 Drop 吞掉。
                lease.finish(frame, cancelled)
            }
        })();
        self.lane.observe(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_geometry_descriptors(
        desktop: RECT,
        rotation: DXGI_MODE_ROTATION,
    ) -> (DXGI_OUTPUT_DESC, DXGI_OUTDUPL_DESC) {
        (
            DXGI_OUTPUT_DESC {
                DesktopCoordinates: desktop,
                Rotation: rotation,
                ..Default::default()
            },
            DXGI_OUTDUPL_DESC {
                ModeDesc: DXGI_MODE_DESC {
                    Width: 1920,
                    Height: 1080,
                    ..Default::default()
                },
                Rotation: rotation,
                ..Default::default()
            },
        )
    }

    #[test]
    fn recording_live_geometry_rejects_same_size_180_degree_handoff() {
        let desktop = RECT {
            left: -1920,
            top: -100,
            right: 0,
            bottom: 980,
        };
        let (mut output, mut duplication) =
            live_geometry_descriptors(desktop, DXGI_MODE_ROTATION_IDENTITY);
        assert!(live_geometry_matches(
            desktop,
            Rotation::Identity,
            (1920, 1080),
            &output,
            &duplication
        ));
        for (current, duplicated) in [
            (DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE180),
            (DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_IDENTITY),
            (DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE180),
        ] {
            output.Rotation = current;
            duplication.Rotation = duplicated;
            assert!(!live_geometry_matches(
                desktop,
                Rotation::Identity,
                (1920, 1080),
                &output,
                &duplication
            ));
        }
    }

    #[test]
    fn recording_live_geometry_accepts_matching_rotations_and_rejects_90_270_swap() {
        for (rotation, native, width, height) in [
            (Rotation::Identity, DXGI_MODE_ROTATION_IDENTITY, 1920, 1080),
            (Rotation::R90, DXGI_MODE_ROTATION_ROTATE90, 1080, 1920),
            (Rotation::R180, DXGI_MODE_ROTATION_ROTATE180, 1920, 1080),
            (Rotation::R270, DXGI_MODE_ROTATION_ROTATE270, 1080, 1920),
        ] {
            let desktop = RECT {
                left: -width,
                top: -100,
                right: 0,
                bottom: height - 100,
            };
            let (output, mut duplication) = live_geometry_descriptors(desktop, native);
            // ModeDesc 不参与 acquired raw 尺寸推断；90/270 的门只比较真实方向。
            duplication.ModeDesc.Width = width as u32;
            duplication.ModeDesc.Height = height as u32;
            assert!(live_geometry_matches(
                desktop,
                rotation,
                (1920, 1080),
                &output,
                &duplication
            ));
        }
        let desktop = RECT {
            left: -1080,
            top: 0,
            right: 0,
            bottom: 1920,
        };
        let (output, duplication) =
            live_geometry_descriptors(desktop, DXGI_MODE_ROTATION_ROTATE270);
        assert!(!live_geometry_matches(
            desktop,
            Rotation::R90,
            (1920, 1080),
            &output,
            &duplication
        ));
        let (output, duplication) = live_geometry_descriptors(desktop, DXGI_MODE_ROTATION_ROTATE90);
        assert!(!live_geometry_matches(
            desktop,
            Rotation::R270,
            (1920, 1080),
            &output,
            &duplication
        ));
    }

    #[test]
    fn recording_live_geometry_checks_snapshot_geometry_and_keeps_unspecified_baseline() {
        let desktop = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let (output, duplication) = live_geometry_descriptors(desktop, DXGI_MODE_ROTATION_IDENTITY);
        let mut moved = output;
        moved.DesktopCoordinates.left = 1;
        moved.DesktopCoordinates.right = 1921;
        assert!(!live_geometry_matches(
            desktop,
            Rotation::Identity,
            (1920, 1080),
            &moved,
            &duplication
        ));
        for raw_size in [(1922, 1080), (1920, 1082), (0, 1080)] {
            assert!(!live_geometry_matches(
                desktop,
                Rotation::Identity,
                raw_size,
                &output,
                &duplication
            ));
        }
        for (current, duplicated) in [
            (
                DXGI_MODE_ROTATION_UNSPECIFIED,
                DXGI_MODE_ROTATION_UNSPECIFIED,
            ),
            (DXGI_MODE_ROTATION_UNSPECIFIED, DXGI_MODE_ROTATION_IDENTITY),
            (DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED),
        ] {
            let (mut output, mut duplication) = (output, duplication);
            output.Rotation = current;
            duplication.Rotation = duplicated;
            assert!(live_geometry_matches(
                desktop,
                Rotation::Identity,
                (1920, 1080),
                &output,
                &duplication
            ));
        }
        for rotation in [DXGI_MODE_ROTATION(99), DXGI_MODE_ROTATION(-1)] {
            let (mut output, mut duplication) = (output, duplication);
            output.Rotation = rotation;
            assert!(!live_geometry_matches(
                desktop,
                Rotation::Identity,
                (1920, 1080),
                &output,
                &duplication
            ));
            output.Rotation = DXGI_MODE_ROTATION_IDENTITY;
            duplication.Rotation = rotation;
            assert!(!live_geometry_matches(
                desktop,
                Rotation::Identity,
                (1920, 1080),
                &output,
                &duplication
            ));
        }
    }

    fn initial_failure(operation: &'static str, code: windows::core::HRESULT) -> Box<dyn Error> {
        initial_capture_error(
            operation,
            operation.into(),
            windows::core::Error::from_hresult(code),
        )
    }

    #[test]
    fn refresh_fallback_only_accepts_unacquired_dxgi_compatibility_failures() {
        for code in [
            DXGI_ERROR_UNSUPPORTED,
            DXGI_ERROR_NOT_CURRENTLY_AVAILABLE,
            DXGI_ERROR_SESSION_DISCONNECTED,
            windows::Win32::Foundation::E_ACCESSDENIED,
            windows::Win32::Foundation::E_NOTIMPL,
        ] {
            assert!(can_fallback_refresh(
                initial_failure("DuplicateOutput", code).as_ref()
            ));
            for operation in ["ReleaseFrame", "GetData", "Readback"] {
                assert!(!can_fallback_refresh(
                    initial_failure(operation, code).as_ref()
                ));
            }
            assert!(!can_fallback_refresh(&windows::core::Error::from_hresult(
                code
            )));
        }
        for code in [
            DXGI_ERROR_WAIT_TIMEOUT,
            DXGI_ERROR_DEVICE_REMOVED,
            DXGI_ERROR_INVALID_CALL,
            windows::Win32::Foundation::E_OUTOFMEMORY,
        ] {
            assert!(!can_fallback_refresh(
                initial_failure("AcquireNextFrame", code).as_ref()
            ));
        }
        assert!(!can_fallback_refresh(&CaptureCancelled));
    }

    #[test]
    fn refresh_fallback_keeps_a_real_failure_after_initial_recovery() {
        let failure = InitialRecoveryFailure {
            first: initial_failure("DuplicateOutput", DXGI_ERROR_NOT_CURRENTLY_AVAILABLE),
            retry: initial_failure("ReleaseFrame", DXGI_ERROR_UNSUPPORTED),
        };
        assert!(!can_fallback_refresh(&failure));
    }

    #[test]
    fn initial_recovery_rebuilds_once_only_for_unacquired_device_or_access_loss() {
        for code in [
            DXGI_ERROR_ACCESS_LOST,
            DXGI_ERROR_DEVICE_REMOVED,
            DXGI_ERROR_DEVICE_RESET,
            DXGI_ERROR_DRIVER_INTERNAL_ERROR,
        ] {
            for operation in [
                "D3D11CreateDevice",
                "DuplicateOutput",
                "CreateQuery",
                "AcquireNextFrame",
            ] {
                let mut calls = 0;
                let value = capture_initial_with_recovery(
                    || {
                        calls += 1;
                        if calls == 1 {
                            Err(initial_failure(operation, code))
                        } else {
                            Ok(17)
                        }
                    },
                    &|| false,
                )
                .unwrap();
                assert_eq!(value, 17);
                assert_eq!(calls, 2, "{operation}: {code:?}");
            }
        }
    }

    #[test]
    fn initial_recovery_keeps_both_actual_failures_and_never_starts_a_third_attempt() {
        let mut calls = 0;
        let error = capture_initial_with_recovery::<()>(
            || {
                calls += 1;
                Err(initial_failure(
                    "AcquireNextFrame",
                    if calls == 1 {
                        DXGI_ERROR_ACCESS_LOST
                    } else {
                        DXGI_ERROR_DEVICE_REMOVED
                    },
                ))
            },
            &|| false,
        )
        .unwrap_err();
        assert_eq!(calls, 2);
        let failure = error.downcast_ref::<InitialRecoveryFailure>().unwrap();
        assert_eq!(
            failure
                .first
                .downcast_ref::<InitialCaptureError>()
                .unwrap()
                .source
                .code(),
            DXGI_ERROR_ACCESS_LOST
        );
        assert_eq!(
            failure
                .retry
                .downcast_ref::<InitialCaptureError>()
                .unwrap()
                .source
                .code(),
            DXGI_ERROR_DEVICE_REMOVED
        );
        assert_eq!(
            error.source().unwrap().to_string(),
            failure.retry.to_string()
        );
        assert!(error.to_string().contains("仅一次重建"));
    }

    #[test]
    fn initial_recovery_does_not_reclassify_timeout_permissions_memory_or_lease_errors() {
        for (operation, code) in [
            ("AcquireNextFrame", DXGI_ERROR_WAIT_TIMEOUT),
            (
                "DuplicateOutput",
                windows::Win32::Foundation::E_ACCESSDENIED,
            ),
            ("CreateQuery", windows::Win32::Foundation::E_OUTOFMEMORY),
            ("DuplicateOutput", DXGI_ERROR_NOT_CURRENTLY_AVAILABLE),
            ("AcquireNextFrame", DXGI_ERROR_DEVICE_HUNG),
            ("ReleaseFrame", DXGI_ERROR_ACCESS_LOST),
            ("GetData", DXGI_ERROR_DEVICE_REMOVED),
            ("Readback", DXGI_ERROR_DEVICE_RESET),
        ] {
            let mut calls = 0;
            let error = capture_initial_with_recovery::<()>(
                || {
                    calls += 1;
                    Err(initial_failure(operation, code))
                },
                &|| false,
            )
            .unwrap_err();
            assert_eq!(calls, 1, "{operation}: {code:?}");
            assert_eq!(
                error
                    .downcast_ref::<InitialCaptureError>()
                    .unwrap()
                    .source
                    .code(),
                code
            );
        }
        let mut calls = 0;
        let error = capture_initial_with_recovery::<()>(
            || {
                calls += 1;
                Err(windows::core::Error::from_hresult(DXGI_ERROR_ACCESS_LOST).into())
            },
            &|| false,
        )
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(error.is::<windows::core::Error>());
    }

    #[test]
    fn initial_recovery_checks_the_same_cancellation_before_retry_and_after_success() {
        use std::cell::Cell;
        let cancelled = Cell::new(true);
        let calls = Cell::new(0);
        let error = capture_initial_with_recovery::<()>(
            || {
                calls.set(calls.get() + 1);
                Ok(())
            },
            &|| cancelled.get(),
        )
        .unwrap_err();
        assert_eq!(calls.get(), 0);
        assert!(error.is::<CaptureCancelled>());
        cancelled.set(false);
        let error = capture_initial_with_recovery::<()>(
            || {
                calls.set(calls.get() + 1);
                cancelled.set(true);
                Err(initial_failure("AcquireNextFrame", DXGI_ERROR_ACCESS_LOST))
            },
            &|| cancelled.get(),
        )
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert!(error.is::<InitialCaptureError>()); // 不隐藏同遇的真实失败。
        cancelled.set(false);
        calls.set(0);
        let error = capture_initial_with_recovery::<()>(
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    Err(initial_failure("DuplicateOutput", DXGI_ERROR_ACCESS_LOST))
                } else {
                    cancelled.set(true);
                    Ok(())
                }
            },
            &|| cancelled.get(),
        )
        .unwrap_err();
        assert_eq!(calls.get(), 2);
        assert!(error.is::<CaptureCancelled>());
    }

    #[test]
    fn initial_recovery_drops_partial_batch_before_reenumeration_and_never_retakes_success() {
        use std::{cell::Cell, rc::Rc};
        struct Partial(Rc<Cell<usize>>);
        impl Drop for Partial {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let drops = Rc::new(Cell::new(0));
        let mut attempts = 0;
        let actual = capture_initial_with_recovery(
            || {
                attempts += 1;
                if attempts == 1 {
                    let _partial_old_output = Partial(drops.clone());
                    Err(initial_failure("AcquireNextFrame", DXGI_ERROR_ACCESS_LOST))
                } else {
                    assert_eq!(drops.get(), 1);
                    Ok(crate::geometry::ScreenRect {
                        left: -3200,
                        top: -1800,
                        width: 5120,
                        height: 2880,
                    })
                }
            },
            &|| false,
        )
        .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(actual.left, -3200);
        let mut calls = 0;
        let value = capture_initial_with_recovery(
            || {
                calls += 1;
                Ok(actual)
            },
            &|| false,
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(value, actual);
    }

    #[test]
    fn identity_readback_rows_skip_padding_and_set_opaque_alpha() {
        let source = [
            1, 2, 3, 0, 4, 5, 6, 128, 201, 202, 203, 204, // 第一行及 padding
            7, 8, 9, 64, 10, 11, 12, 255, // 最后一行无需尾部 padding
        ];
        let mut target = [0; 16];
        copy_identity_bgra_rows(&source, 12, &mut target, 8).unwrap();
        assert_eq!(
            target,
            [1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
    }

    #[test]
    fn identity_readback_subrectangle_uses_cropped_origin_and_exact_row_width() {
        let row_pitch = 24;
        let mut source = vec![222; row_pitch * 4];
        for y in 0..4 {
            for x in 0..5 {
                let offset = y * row_pitch + x * 4;
                source[offset..offset + 4].copy_from_slice(&[
                    x as u8,
                    y as u8,
                    (x + y * 5) as u8,
                    17,
                ]);
            }
        }
        // 模拟 staging 的裁剪原点：2×2 子矩形从原始 (2, 1) 开始，行宽仍带 padding。
        let start = row_pitch + 2 * 4;
        let mut target = [0; 16];
        copy_identity_bgra_rows(&source[start..], row_pitch, &mut target, 8).unwrap();
        assert_eq!(
            target,
            [2, 1, 7, 255, 3, 1, 8, 255, 2, 2, 12, 255, 3, 2, 13, 255]
        );
    }

    #[test]
    fn identity_readback_rejects_short_rows_and_buffers_before_writing() {
        let mut target = [123; 16];
        for (source, pitch, row_bytes) in [
            (&[0; 16][..], 4, 8),
            (&[0; 19][..], 12, 8),
            (&[0; 32][..], 12, 12),
            (&[0; 16][..], 8, 0),
            (&[0; 16][..], 8, 6),
            (&[0; 16][..], usize::MAX, 8),
        ] {
            assert!(copy_identity_bgra_rows(source, pitch, &mut target, row_bytes).is_err());
            assert_eq!(target, [123; 16]);
        }
    }

    /// 实机：裁剪采集后按选区连续刷新，像素应与首帧一致；HDR 屏上覆盖 FP16 换算的坐标偏移。
    #[test]
    #[ignore = "读取真实桌面"]
    fn live_region_refresh_matches_initial_capture_on_every_output() {
        let never = || false;
        for (_, _, desc) in targets().unwrap() {
            let d = desc.DesktopCoordinates;
            let (width, height) = ((d.right - d.left) as u32, (d.bottom - d.top) as u32);
            let region = ScreenRect {
                left: d.left + (width / 4) as i32,
                top: d.top + (height / 4) as i32,
                width: width / 2,
                height: height / 2,
            };
            let mut snapshots = Snapshot::capture_region_cancellable(region, &never).unwrap();
            let snapshot = snapshots.remove(0);
            let inner = Rect {
                x: width * 3 / 8,
                y: height * 3 / 8,
                width: width / 4,
                height: height / 4,
            };
            let before = snapshot.read_region(inner).unwrap();
            let live = LiveScreen::new(snapshot).unwrap();
            let s = &live.snapshot;
            unsafe {
                let mut td = D3D11_TEXTURE2D_DESC::default();
                s.texture().GetDesc(&mut td);
                td.BindFlags = 0;
                let zeros = vec![0_u8; (td.Width * td.Height * 4) as usize];
                let mut blank = None;
                s.device
                    .CreateTexture2D(
                        &td,
                        Some(&D3D11_SUBRESOURCE_DATA {
                            pSysMem: zeros.as_ptr().cast(),
                            SysMemPitch: td.Width * 4,
                            SysMemSlicePitch: 0,
                        }),
                        Some(&mut blank),
                    )
                    .unwrap();
                s.context.CopyResource(s.texture(), &blank.unwrap());
            }
            // 新复制器的首帧通常立即可取；清零后等到选区被重新写入。
            let start = Instant::now();
            let after = loop {
                live.refresh_region_cancellable(inner, &never).unwrap();
                let image = s.read_region(inner).unwrap();
                let written = image.bgra.chunks_exact(4).any(|p| p[..3] != [0, 0, 0]);
                if written || start.elapsed().as_secs() >= 2 {
                    break image;
                }
                std::thread::sleep(std::time::Duration::from_millis(16));
            };
            let close = before
                .bgra
                .chunks_exact(4)
                .zip(after.bgra.chunks_exact(4))
                .filter(|(a, b)| a.iter().zip(b.iter()).all(|(x, y)| x.abs_diff(*y) <= 2))
                .count();
            let total = before.bgra.len() / 4;
            assert!(
                close * 100 >= total * 97,
                "屏幕原点 {},{}：{close}/{total} 像素一致",
                d.left,
                d.top
            );
        }
    }
}
