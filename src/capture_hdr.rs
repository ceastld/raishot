//! HDR 输出的桌面复制。系统在 HDR 下用 FP16 scRGB 合成，SDR 内容被乘上“SDR 内容亮度”
//! 白点倍数；旧版复制接口把它直接截断成 8 位，截图整体过曝发白。HDR 输出改要 FP16 帧，
//! 在 GPU 上除回白点并编码为 sRGB，写进同一张 BGRA8 自有纹理，下游读回、录屏不感知差异。
//! SDR 输出、取不到白点或新接口失败时仍走原来的复制路径。
#[path = "capture_hdr_shaders.rs"]
mod shaders;

use crate::geometry::Rect;
use std::mem::size_of;
use windows::{
    core::Interface,
    Win32::Graphics::{
        Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
        Direct3D11::*,
        Dxgi::{Common::*, *},
    },
};

/// 一块 HDR 输出在当前设备上的换算着色器和白点。
pub(super) struct SdrConverter {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    params: ID3D11Buffer,
    inv_sdr_white: f32,
}

/// 与 `capture_hdr.hlsl` 的 `Params` 常量缓冲逐字段对应。
#[repr(C)]
struct Params {
    source_offset: [i32; 2],
    inv_sdr_white: f32,
    unused: f32,
}

/// 一帧换算所需的视图；创建可能失败，所以在提交 GPU 命令前准备好。
pub(super) struct SdrPass<'a> {
    converter: &'a SdrConverter,
    staging: Option<(ID3D11Texture2D, Rect)>,
    source: ID3D11ShaderResourceView,
    target: ID3D11RenderTargetView,
    source_origin: (u32, u32),
    target_rect: Rect,
}

/// 创建桌面复制器。`allow_fp16` 为假时（自有纹理不能作渲染目标）只用旧接口。
pub(super) fn duplicate(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
    allow_fp16: bool,
) -> windows::core::Result<(IDXGIOutputDuplication, Option<SdrConverter>)> {
    if allow_fp16 {
        if let Some((duplication, converter)) = duplicate_fp16(output, device) {
            return Ok((duplication, Some(converter)));
        }
    }
    Ok((unsafe { output.DuplicateOutput(device)? }, None))
}

fn duplicate_fp16(
    output: &IDXGIOutput1,
    device: &ID3D11Device,
) -> Option<(IDXGIOutputDuplication, SdrConverter)> {
    let output6: IDXGIOutput6 = output.cast().ok()?;
    let desc = unsafe { output6.GetDesc1() }.ok()?;
    if desc.ColorSpace != DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 {
        return None;
    }
    let trace = std::env::var_os("RUSTSHOT_TRACE").is_some();
    let Some(white) = sdr_white_scale(&desc.DeviceName) else {
        if trace {
            eprintln!("HDR 输出未取得 SDR 白点，沿用 8 位桌面复制");
        }
        return None;
    };
    let result = (|| -> windows::core::Result<_> {
        let converter = SdrConverter::new(device, white)?;
        let duplication = unsafe {
            output6.DuplicateOutput1(
                device,
                0,
                &[DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM],
            )?
        };
        Ok((duplication, converter))
    })();
    if trace {
        match &result {
            Ok(_) => eprintln!(
                "HDR 输出：SDR 白点 {:.0} nits，使用 FP16 桌面复制",
                white * 80.
            ),
            Err(error) => eprintln!("HDR 输出 FP16 桌面复制不可用，沿用 8 位：{error}"),
        }
    }
    result.ok()
}

impl SdrConverter {
    fn new(device: &ID3D11Device, sdr_white: f32) -> windows::core::Result<Self> {
        unsafe {
            let mut vertex = None;
            device.CreateVertexShader(
                &shaders::VERTEX_SHADER,
                None::<&ID3D11ClassLinkage>,
                Some(&mut vertex),
            )?;
            let mut pixel = None;
            device.CreatePixelShader(
                &shaders::PIXEL_SHADER,
                None::<&ID3D11ClassLinkage>,
                Some(&mut pixel),
            )?;
            let mut params = None;
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: size_of::<Params>() as u32,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut params),
            )?;
            Ok(Self {
                vertex: vertex.ok_or_else(missing)?,
                pixel: pixel.ok_or_else(missing)?,
                params: params.ok_or_else(missing)?,
                inv_sdr_white: 1. / sdr_white,
            })
        }
    }

    /// 准备把 `source` 的 `source_rect`（原始坐标）换算写入 `target` 的 `target_rect`。
    pub(super) fn prepare(
        &self,
        device: &ID3D11Device,
        source: &ID3D11Texture2D,
        source_rect: Rect,
        target: &ID3D11Texture2D,
        target_rect: Rect,
    ) -> windows::core::Result<SdrPass<'_>> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            source.GetDesc(&mut desc);
            // 复制帧通常可直接采样；驱动不给采样绑定时先拷进同尺寸的中转纹理。
            let (view_source, staging, source_origin) =
                if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 != 0 {
                    (source.clone(), None, (source_rect.x, source_rect.y))
                } else {
                    let mut staging = None;
                    device.CreateTexture2D(
                        &D3D11_TEXTURE2D_DESC {
                            Width: source_rect.width,
                            Height: source_rect.height,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: desc.Format,
                            SampleDesc: DXGI_SAMPLE_DESC {
                                Count: 1,
                                Quality: 0,
                            },
                            Usage: D3D11_USAGE_DEFAULT,
                            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                            ..Default::default()
                        },
                        None,
                        Some(&mut staging),
                    )?;
                    let staging = staging.ok_or_else(missing)?;
                    (staging.clone(), Some((staging, source_rect)), (0, 0))
                };
            let mut view = None;
            device.CreateShaderResourceView(&view_source, None, Some(&mut view))?;
            let mut render = None;
            device.CreateRenderTargetView(target, None, Some(&mut render))?;
            Ok(SdrPass {
                converter: self,
                staging,
                source: view.ok_or_else(missing)?,
                target: render.ok_or_else(missing)?,
                source_origin,
                target_rect,
            })
        }
    }
}

impl SdrPass<'_> {
    /// 只记录 GPU 命令；完成与否由调用方的帧租约等待。
    pub(super) fn draw(&self, context: &ID3D11DeviceContext, source: &ID3D11Texture2D) {
        let converter = self.converter;
        let target = self.target_rect;
        let params = Params {
            source_offset: [
                self.source_origin.0 as i32 - target.x as i32,
                self.source_origin.1 as i32 - target.y as i32,
            ],
            inv_sdr_white: converter.inv_sdr_white,
            unused: 0.,
        };
        unsafe {
            if let Some((staging, rect)) = &self.staging {
                context.CopySubresourceRegion(
                    staging,
                    0,
                    0,
                    0,
                    0,
                    source,
                    0,
                    Some(&D3D11_BOX {
                        left: rect.x,
                        top: rect.y,
                        front: 0,
                        right: rect.x + rect.width,
                        bottom: rect.y + rect.height,
                        back: 1,
                    }),
                );
            }
            context.UpdateSubresource(
                &converter.params,
                0,
                None,
                (&params as *const Params).cast(),
                0,
                0,
            );
            context.IASetInputLayout(None::<&ID3D11InputLayout>);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&converter.vertex, None);
            context.PSSetShader(&converter.pixel, None);
            context.PSSetShaderResources(0, Some(&[Some(self.source.clone())]));
            context.PSSetConstantBuffers(0, Some(&[Some(converter.params.clone())]));
            context.RSSetState(None::<&ID3D11RasterizerState>);
            context.OMSetBlendState(None::<&ID3D11BlendState>, None, u32::MAX);
            context.OMSetRenderTargets(
                Some(&[Some(self.target.clone())]),
                None::<&ID3D11DepthStencilView>,
            );
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: target.x as f32,
                TopLeftY: target.y as f32,
                Width: target.width as f32,
                Height: target.height as f32,
                MinDepth: 0.,
                MaxDepth: 1.,
            }]));
            context.Draw(3, 0);
            // 解除绑定，自有纹理随后还要作拷贝源或视频处理输入。
            context.PSSetShaderResources(0, Some(&[None]));
            context.OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
        }
    }
}

fn missing() -> windows::core::Error {
    windows::core::Error::from_hresult(windows::Win32::Foundation::E_POINTER)
}

// DisplayConfig 只用到 SDR 白点查询；按 Win32 布局手写，不新增 windows feature。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Luid {
    low: u32,
    high: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PathInfo {
    source_adapter: Luid,
    source_id: u32,
    source_mode: u32,
    source_flags: u32,
    target_adapter: Luid,
    target_id: u32,
    target_mode: u32,
    // 输出技术、旋转、缩放、刷新率（两项）、扫描方式、可用性、状态。
    target_rest: [u32; 8],
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ModeInfo([u64; 8]);

#[repr(C)]
#[derive(Clone, Copy)]
struct DeviceInfoHeader {
    kind: u32,
    size: u32,
    adapter: Luid,
    id: u32,
}

#[repr(C)]
struct SourceName {
    header: DeviceInfoHeader,
    gdi_name: [u16; 32],
}

#[repr(C)]
struct SdrWhiteLevel {
    header: DeviceInfoHeader,
    level: u32,
}

const _: () = assert!(size_of::<PathInfo>() == 72);
const _: () = assert!(size_of::<ModeInfo>() == 64);
const _: () = assert!(size_of::<SourceName>() == 84);
const _: () = assert!(size_of::<SdrWhiteLevel>() == 24);

const QDC_ONLY_ACTIVE_PATHS: u32 = 2;
const GET_SOURCE_NAME: u32 = 1;
const GET_SDR_WHITE_LEVEL: u32 = 11;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;

#[link(name = "user32")]
unsafe extern "system" {
    fn GetDisplayConfigBufferSizes(flags: u32, paths: *mut u32, modes: *mut u32) -> i32;
    fn QueryDisplayConfig(
        flags: u32,
        path_count: *mut u32,
        paths: *mut PathInfo,
        mode_count: *mut u32,
        modes: *mut ModeInfo,
        topology: *mut u32,
    ) -> i32;
    fn DisplayConfigGetDeviceInfo(packet: *mut DeviceInfoHeader) -> i32;
}

/// 该 GDI 设备名对应输出的 SDR 白点，以 scRGB 1.0（80 nits）为单位。
fn sdr_white_scale(device_name: &[u16; 32]) -> Option<f32> {
    let paths = active_paths()?;
    let name_len = device_name.iter().position(|&c| c == 0).unwrap_or(32);
    let path = paths.iter().find(|path| unsafe {
        let mut source = SourceName {
            header: DeviceInfoHeader {
                kind: GET_SOURCE_NAME,
                size: size_of::<SourceName>() as u32,
                adapter: path.source_adapter,
                id: path.source_id,
            },
            gdi_name: [0; 32],
        };
        DisplayConfigGetDeviceInfo(&mut source.header) == 0
            && source.gdi_name[..name_len] == device_name[..name_len]
            && source.gdi_name.get(name_len).is_none_or(|&c| c == 0)
    })?;
    let mut white = SdrWhiteLevel {
        header: DeviceInfoHeader {
            kind: GET_SDR_WHITE_LEVEL,
            size: size_of::<SdrWhiteLevel>() as u32,
            adapter: path.target_adapter,
            id: path.target_id,
        },
        level: 0,
    };
    if unsafe { DisplayConfigGetDeviceInfo(&mut white.header) } != 0 {
        return None;
    }
    sdr_white_from_level(white.level)
}

/// 系统以 1000 表示 80 nits；超出合理范围的值视为查询无效。
fn sdr_white_from_level(level: u32) -> Option<f32> {
    let scale = level as f32 / 1000.;
    (1.0..=12.5).contains(&scale).then_some(scale)
}

fn active_paths() -> Option<Vec<PathInfo>> {
    // 两次调用之间拓扑可能变化，缓冲不足时重新取尺寸。
    for _ in 0..3 {
        let (mut path_count, mut mode_count) = (0, 0);
        if unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        } != 0
        {
            return None;
        }
        let mut paths = vec![PathInfo::default(); path_count as usize];
        let mut modes = vec![ModeInfo::default(); mode_count as usize];
        match unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        } {
            0 => {
                paths.truncate(path_count as usize);
                return Some(paths);
            }
            ERROR_INSUFFICIENT_BUFFER => continue,
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hdr_sdr_white_level_uses_80_nit_units_and_rejects_nonsense() {
        assert_eq!(sdr_white_from_level(1000), Some(1.));
        assert_eq!(sdr_white_from_level(3500), Some(3.5));
        assert_eq!(sdr_white_from_level(0), None);
        assert_eq!(sdr_white_from_level(999), None);
        assert_eq!(sdr_white_from_level(20_000), None);
    }

    #[test]
    fn hdr_shader_bytecode_is_dxbc() {
        for code in [&shaders::VERTEX_SHADER[..], &shaders::PIXEL_SHADER[..]] {
            assert_eq!(&code[..4], b"DXBC");
            assert_eq!(
                u32::from_le_bytes(code[24..28].try_into().unwrap()) as usize,
                code.len()
            );
        }
    }
}
