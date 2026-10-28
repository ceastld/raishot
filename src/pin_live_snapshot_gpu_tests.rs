use windows::{
    core::Interface,
    Win32::{
        Foundation::{HMODULE, HWND},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
    },
};

/// 用真实 SDR 交换链提供测试像素，覆盖与 GDI 绘制不同的窗口合成路径。
pub(super) struct SdrSurface {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap_chain: IDXGISwapChain1,
    row_pitch: u32,
    byte_len: usize,
}

impl SdrSurface {
    pub(super) fn new(window: HWND, width: u32, height: u32) -> crate::Result<Self> {
        let row_pitch = width.checked_mul(4).ok_or("GPU 测试图像宽度溢出")?;
        let byte_len = (row_pitch as usize)
            .checked_mul(height as usize)
            .filter(|&len| len > 0)
            .ok_or("GPU 测试图像尺寸无效")?;
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.ok_or("GPU 测试未返回设备")?;
        let context = context.ok_or("GPU 测试未返回上下文")?;
        let dxgi: IDXGIDevice = device.cast()?;
        let swap_chain = unsafe {
            let adapter = dxgi.GetAdapter()?;
            let factory: IDXGIFactory2 = adapter.GetParent()?;
            factory.CreateSwapChainForHwnd(
                &device,
                window,
                &DXGI_SWAP_CHAIN_DESC1 {
                    Width: width,
                    Height: height,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    Scaling: DXGI_SCALING_STRETCH,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                    AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                    ..Default::default()
                },
                None,
                None::<&IDXGIOutput>,
            )?
        };
        Ok(Self {
            _device: device,
            context,
            swap_chain,
            row_pitch,
            byte_len,
        })
    }

    pub(super) fn present(&self, bgra: &[u8]) -> crate::Result<()> {
        if bgra.len() != self.byte_len {
            return Err("GPU 测试像素数量与交换链尺寸不符".into());
        }
        unsafe {
            // 每次重新取当前后备缓冲，避免翻转后向已经提交的缓冲写入。
            let buffer: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;
            self.context.UpdateSubresource(
                &buffer,
                0,
                None,
                bgra.as_ptr().cast(),
                self.row_pitch,
                0,
            );
            self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }
}
