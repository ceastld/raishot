//! 录屏的 GPU 色彩转换与有界 DXGI 样本池。
//! BGRA 输入在同一个 D3D11 设备上转换为 BT.709 / limited-range NV12，
//! 交给 Media Foundation；样本最后一个引用释放后才允许池复用其纹理。
use crate::{geometry::Rect, media_foundation::frame_resize::FrameResizeGeometry, Result};
use std::{
    mem::ManuallyDrop,
    time::{Duration, Instant},
};
use windows::{
    core::{IUnknown, Interface},
    Win32::{
        Foundation::{HMODULE, RECT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0},
            Direct3D10::ID3D10Multithread,
            Direct3D11::*,
            Dxgi::{Common::*, IDXGIAdapter},
        },
        Media::MediaFoundation::*,
    },
};

const INITIAL_SAMPLES: u32 = 3;
const MAX_SAMPLES: u32 = 6;
const SAMPLE_WAIT_LIMIT: Duration = Duration::from_secs(1);

fn ticks(duration: Duration) -> i64 {
    (duration.as_nanos() / 100).min(i64::MAX as u128) as i64
}

/// 编解码专用设备，不接触桌面、窗口或 GPUI；调用线程须已初始化 COM / MF。
pub(crate) fn create_video_device() -> Result<ID3D11Device> {
    unsafe {
        let mut device = None;
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let device = device.ok_or("录屏 D3D11 未返回设备")?;
        protect_device(&device)?;
        Ok(device)
    }
}

pub(crate) fn protect_device(device: &ID3D11Device) -> Result<()> {
    unsafe {
        // MF 编码器会在自己的工作线程使用同一 immediate context。
        // SetMultithreadProtected 返回旧状态，false 不是失败。
        let multithread: ID3D10Multithread = device.cast()?;
        let _ = multithread.SetMultithreadProtected(true);
        if !multithread.GetMultithreadProtected().as_bool() {
            return Err("录屏 D3D11 无法开启多线程保护".into());
        }
        Ok(())
    }
}

fn nv12_media_type(width: u32, height: u32, fps: u32) -> Result<IMFMediaType> {
    unsafe {
        let media = MFCreateMediaType()?;
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        media.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(width) << 32) | u64::from(height),
        )?;
        media.SetUINT64(&MF_MT_FRAME_RATE, (u64::from(fps.max(1)) << 32) | 1)?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media.SetUINT32(&MF_MT_DEFAULT_STRIDE, width)?;
        media.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        media.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        media.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        Ok(media)
    }
}

struct OutputView {
    texture: ID3D11Texture2D,
    subresource: u32,
    view: ID3D11VideoProcessorOutputView,
}

/// 只在录屏工作线程使用；调用方须先完成 Sink Writer，再释放本对象与 MF。
/// 构造失败由调用方回退 CPU / 软件编码，不把可选 GPU 能力当作运行前提。
pub(crate) struct GpuEncoderFrames {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    manager: IMFDXGIDeviceManager,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input_texture: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    allocator: IMFVideoSampleAllocatorEx,
    output_views: Vec<OutputView>,
    source_width: u32,
    source_height: u32,
    width: u32,
    height: u32,
    frame_index: u32,
}

impl GpuEncoderFrames {
    pub(crate) fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        Self::new_with_device(width, height, fps, &create_video_device()?)
    }

    /// 可复用录屏采集器同一适配器上的设备，避免跨设备拷贝与 CPU 读回。
    pub(crate) fn new_with_device(
        width: u32,
        height: u32,
        fps: u32,
        device: &ID3D11Device,
    ) -> Result<Self> {
        // 旧 identity 入口保留 D3D11 原有尺寸范围，不收紧为实验缩放的项目预算。
        Self::create(width, height, width, height, fps, device)
    }

    /// 在一次 VideoProcessorBlt 中完成源 BGRA → 目标 NV12 的降采样与色彩转换。
    pub(crate) fn new_resized(geometry: FrameResizeGeometry, fps: u32) -> Result<Self> {
        let geometry = FrameResizeGeometry::new(
            geometry.source_width,
            geometry.source_height,
            geometry.target_width,
            geometry.target_height,
        )?;
        Self::new_resized_with_device(geometry, fps, &create_video_device()?)
    }

    pub(crate) fn new_resized_with_device(
        geometry: FrameResizeGeometry,
        fps: u32,
        device: &ID3D11Device,
    ) -> Result<Self> {
        // 报告用公开字段不能绕过偶数、逐轴不放大及源/目标预算校验。
        let geometry = FrameResizeGeometry::new(
            geometry.source_width,
            geometry.source_height,
            geometry.target_width,
            geometry.target_height,
        )?;
        Self::create(
            geometry.source_width,
            geometry.source_height,
            geometry.target_width,
            geometry.target_height,
            fps,
            device,
        )
    }

    fn create(
        source_width: u32,
        source_height: u32,
        width: u32,
        height: u32,
        fps: u32,
        device: &ID3D11Device,
    ) -> Result<Self> {
        let dimensions = [source_width, source_height, width, height];
        if dimensions
            .iter()
            .any(|&size| size < 2 || !size.is_multiple_of(2))
        {
            return Err("GPU 编码尺寸必须为不小于 2 的偶数".into());
        }
        if dimensions
            .iter()
            .any(|&size| size > D3D11_REQ_TEXTURE2D_U_OR_V_DIMENSION)
        {
            return Err("GPU 编码尺寸超过 D3D11 纹理上限".into());
        }
        unsafe {
            protect_device(device)?;
            let context = device.GetImmediateContext()?;
            let video_device: ID3D11VideoDevice = device.cast()?;
            let video_context: ID3D11VideoContext = context.cast()?;
            let rate = DXGI_RATIONAL {
                Numerator: fps.max(1),
                Denominator: 1,
            };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: source_width,
                InputHeight: source_height,
                OutputFrameRate: rate,
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&content)?;
            if enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)?
                & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32
                == 0
                || enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)?
                    & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32
                    == 0
            {
                return Err("显卡视频处理器不支持 BGRA → NV12".into());
            }
            let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
            enumerator.GetVideoProcessorCaps(&mut caps)?;
            let mut processor_index = None;
            let mut reference_cost = u32::MAX;
            for index in 0..caps.RateConversionCapsCount {
                let mut rate_caps = D3D11_VIDEO_PROCESSOR_RATE_CONVERSION_CAPS::default();
                enumerator.GetVideoProcessorRateConversionCaps(index, &mut rate_caps)?;
                // Past/Future 是最佳去交错/变帧率处理的参考需求；逐行同帧率
                // 色彩转换不启用这些处理，也不因驱动同时提供它们而拒绝 GPU。
                let cost = rate_caps.PastFrames.saturating_add(rate_caps.FutureFrames);
                if cost < reference_cost {
                    processor_index = Some(index);
                    reference_cost = cost;
                }
            }
            let processor = video_device.CreateVideoProcessor(
                &enumerator,
                processor_index.ok_or("显卡未提供可用的视频处理器")?,
            )?;
            let source_bounds = RECT {
                left: 0,
                top: 0,
                right: source_width as i32,
                bottom: source_height as i32,
            };
            let target_bounds = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            video_context.VideoProcessorSetStreamSourceRect(
                &processor,
                0,
                true,
                Some(&source_bounds),
            );
            video_context.VideoProcessorSetStreamDestRect(
                &processor,
                0,
                true,
                Some(&target_bounds),
            );
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target_bounds));
            // d3d11.h 位域顺序：Usage、RGB_Range、YCbCr_Matrix、xvYCC、Nominal_Range。
            // BGRA 是 full range，NV12 是 BT.709 / 16–235；输出 media type 同样声明。
            let input_color = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: 1 | (2 << 4),
            };
            let output_color = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: 1 | (1 << 2) | (1 << 4),
            };
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input_color);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &output_color);

            let input_desc = D3D11_TEXTURE2D_DESC {
                Width: source_width,
                Height: source_height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut input_texture = None;
            device.CreateTexture2D(&input_desc, None, Some(&mut input_texture))?;
            let input_texture = input_texture.ok_or("无法创建 GPU 色彩转换输入纹理")?;
            let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input_view = None;
            video_device.CreateVideoProcessorInputView(
                &input_texture,
                &enumerator,
                &input_view_desc,
                Some(&mut input_view),
            )?;
            let input_view = input_view.ok_or("无法创建 GPU 色彩转换输入视图")?;
            let mut reset_token = 0;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
            let manager = manager.ok_or("无法创建 Media Foundation DXGI 设备管理器")?;
            manager.ResetDevice(device, reset_token)?;
            let mut allocator_raw = std::ptr::null_mut();
            MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocatorEx::IID, &mut allocator_raw)?;
            if allocator_raw.is_null() {
                return Err("无法创建 GPU 编码样本池".into());
            }
            let allocator = IMFVideoSampleAllocatorEx::from_raw(allocator_raw);
            allocator.SetDirectXManager(&manager)?;
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 3)?;
            let attributes = attributes.ok_or("无法创建 GPU 编码样本池属性")?;
            attributes.SetUINT32(&MF_SA_BUFFERS_PER_SAMPLE, 1)?;
            attributes.SetUINT32(&MF_SA_D3D11_USAGE, D3D11_USAGE_DEFAULT.0 as u32)?;
            attributes.SetUINT32(&MF_SA_D3D11_BINDFLAGS, D3D11_BIND_RENDER_TARGET.0 as u32)?;
            allocator.InitializeSampleAllocatorEx(
                INITIAL_SAMPLES,
                MAX_SAMPLES,
                &attributes,
                &nv12_media_type(width, height, fps)?,
            )?;
            let mut frames = Self {
                device: device.clone(),
                context,
                manager,
                video_device,
                video_context,
                enumerator,
                processor,
                input_texture,
                input_view,
                allocator,
                output_views: Vec::with_capacity(MAX_SAMPLES as usize),
                source_width,
                source_height,
                width,
                height,
                frame_index: 0,
            };
            // 提前验证真实 DXGI buffer 与 NV12 output view，初始化失败可安全回退。
            let sample = frames.allocator.AllocateSample()?;
            frames.output_view(&sample)?;
            Ok(frames)
        }
    }

    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub(crate) fn manager(&self) -> &IMFDXGIDeviceManager {
        &self.manager
    }

    /// 兼容需要 CPU 叠加的路径，只上传原 BGRA，不经 CPU 做 RGB → YUV。
    /// source_width 是调用方 BGRA 行跨度，可比逻辑源宽多 1 像素；不按目标尺寸裁源。
    pub(crate) fn frame_from_bgra(
        &mut self,
        bytes: &[u8],
        source_width: u32,
        time: Duration,
        duration: Duration,
    ) -> Result<IMFSample> {
        let source_row = source_width.checked_mul(4).ok_or("录屏帧行跨度溢出")?;
        let needed = (source_row as usize)
            .checked_mul(self.source_height as usize - 1)
            .and_then(|value| value.checked_add(self.source_width as usize * 4))
            .ok_or("录屏帧尺寸溢出")?;
        if source_width < self.source_width || bytes.len() < needed {
            return Err("录屏帧尺寸与 GPU 编码尺寸不一致".into());
        }
        let sample = self.allocate_sample()?;
        unsafe {
            self.context.UpdateSubresource(
                &self.input_texture,
                0,
                None,
                bytes.as_ptr().cast(),
                source_row,
                0,
            );
        }
        self.convert(sample, time, duration)
    }

    /// 同设备 BGRA 纹理按逻辑源尺寸裁剪，再一次 GPU 降采样/转换，不逐帧读回 CPU。
    /// crop 为纹理局部物理坐标；调用者负责在归还桌面帧前等待其 GPU 拷贝完成。
    pub(crate) fn frame_from_texture(
        &mut self,
        texture: &ID3D11Texture2D,
        crop: Rect,
        time: Duration,
        duration: Duration,
    ) -> Result<IMFSample> {
        unsafe {
            let source_device: IUnknown = texture.GetDevice()?.cast()?;
            let device: IUnknown = self.device.cast()?;
            if source_device.as_raw() != device.as_raw() {
                return Err("GPU 录屏帧必须与编码器使用同一 D3D11 设备".into());
            }
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM
                || desc.SampleDesc.Count != 1
                || desc.Usage != D3D11_USAGE_DEFAULT
                || !crop.valid(desc.Width, desc.Height)
                || crop.width < self.source_width
                || crop.height < self.source_height
            {
                return Err("GPU 录屏纹理格式或裁剪范围无效".into());
            }
            let sample = self.allocate_sample()?;
            let bounds = D3D11_BOX {
                left: crop.x,
                top: crop.y,
                front: 0,
                right: crop.x + self.source_width,
                bottom: crop.y + self.source_height,
                back: 1,
            };
            self.context.CopySubresourceRegion(
                &self.input_texture,
                0,
                0,
                0,
                0,
                texture,
                0,
                Some(&bounds),
            );
            self.convert(sample, time, duration)
        }
    }

    fn allocate_sample(&self) -> Result<IMFSample> {
        let started = Instant::now();
        loop {
            match unsafe { self.allocator.AllocateSample() } {
                Ok(sample) => return Ok(sample),
                Err(error) if error.code() == MF_E_SAMPLEALLOCATOR_EMPTY => {
                    if started.elapsed() >= SAMPLE_WAIT_LIMIT {
                        return Err("GPU 编码器持续占用全部 6 个样本，停止增加队列".into());
                    }
                    unsafe { self.device.GetDeviceRemovedReason()? };
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn output_view(&mut self, sample: &IMFSample) -> Result<ID3D11VideoProcessorOutputView> {
        unsafe {
            let buffer = sample.GetBufferByIndex(0)?;
            let dxgi: IMFDXGIBuffer = buffer.cast()?;
            let mut texture_raw = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture_raw)?;
            if texture_raw.is_null() {
                return Err("GPU 样本未返回 NV12 纹理".into());
            }
            let texture = ID3D11Texture2D::from_raw(texture_raw);
            let subresource = dxgi.GetSubresourceIndex()?;
            if let Some(cached) = self.output_views.iter().find(|cached| {
                cached.texture.as_raw() == texture.as_raw() && cached.subresource == subresource
            }) {
                return Ok(cached.view.clone());
            }
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            if desc.Format != DXGI_FORMAT_NV12
                || desc.MipLevels == 0
                || desc.Width < self.width
                || desc.Height < self.height
                || subresource >= desc.MipLevels.saturating_mul(desc.ArraySize)
            {
                return Err("GPU 样本池返回了不兼容的 NV12 纹理".into());
            }
            let mip = subresource % desc.MipLevels;
            let slice = subresource / desc.MipLevels;
            let view_desc = if desc.ArraySize == 1 {
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: mip },
                    },
                }
            } else {
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_VPOV {
                            MipSlice: mip,
                            FirstArraySlice: slice,
                            ArraySize: 1,
                        },
                    },
                }
            };
            let mut view = None;
            self.video_device.CreateVideoProcessorOutputView(
                &texture,
                &self.enumerator,
                &view_desc,
                Some(&mut view),
            )?;
            let view = view.ok_or("无法创建 GPU NV12 输出视图")?;
            if self.output_views.len() >= MAX_SAMPLES as usize {
                return Err("GPU 样本池超出固定的纹理视图上限".into());
            }
            self.output_views.push(OutputView {
                texture,
                subresource,
                view: view.clone(),
            });
            Ok(view)
        }
    }

    fn convert(
        &mut self,
        sample: IMFSample,
        time: Duration,
        duration: Duration,
    ) -> Result<IMFSample> {
        let output_view = self.output_view(&sample)?;
        unsafe {
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                InputFrameOrField: self.frame_index,
                pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
                ..Default::default()
            };
            let result = self.video_context.VideoProcessorBlt(
                &self.processor,
                &output_view,
                self.frame_index,
                std::slice::from_ref(&stream),
            );
            // windows-rs 的 COM 字段为 ManuallyDrop，成功/失败均释放本次克隆。
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
            self.context.Flush();
            self.device.GetDeviceRemovedReason()?;
            // allocator返回可复用纹理但CurrentLength为0；Sink Writer会拒绝空样本。
            // 使用二维缓冲真实连续长度，不假定驱动pitch或宏块填充大小。
            let buffer = sample.GetBufferByIndex(0)?;
            let length = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buffer.SetCurrentLength(length)?;
            sample.SetSampleTime(ticks(time))?;
            sample.SetSampleDuration(ticks(duration).max(1))?;
            self.frame_index = self.frame_index.wrapping_add(1);
            Ok(sample)
        }
    }
}
