// RustShot 本地补丁：复用已通过功能验证的首个 D3D 设备，避免探测成功后丢弃并重建。
// QA 可用 RUSTSHOT_GPUI_LEGACY_DEVICE_PROBE=1 恢复同一版本的原探测路径。
use anyhow::{Context, Result};
use std::time::Instant;
use util::ResultExt;
use windows::Win32::{
    Foundation::HMODULE,
    Graphics::{
        Direct3D::{
            D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
        },
        Direct3D11::{
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
            D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS, D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS,
            D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
        },
        Dxgi::{
            CreateDXGIFactory2, DXGI_CREATE_FACTORY_DEBUG, DXGI_CREATE_FACTORY_FLAGS,
            DXGI_GPU_PREFERENCE_MINIMUM_POWER, IDXGIAdapter1, IDXGIFactory6,
        },
    },
};
use windows::core::Interface;

pub(crate) fn try_to_recover_from_device_lost<T>(
    mut f: impl FnMut() -> Result<T>,
    on_success: impl FnOnce(T),
    on_error: impl FnOnce(),
) {
    let result = (0..5).find_map(|i| {
        if i > 0 {
            // Add a small delay before retrying
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        f().log_err()
    });

    if let Some(result) = result {
        on_success(result);
    } else {
        on_error();
    }
}

#[derive(Clone)]
pub(crate) struct DirectXDevices {
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) dxgi_factory: IDXGIFactory6,
    pub(crate) device: ID3D11Device,
    pub(crate) device_context: ID3D11DeviceContext,
}

impl DirectXDevices {
    pub(crate) fn new() -> Result<Self> {
        let started = Instant::now();
        let legacy_probe = std::env::var("RUSTSHOT_GPUI_LEGACY_DEVICE_PROBE").as_deref() == Ok("1");
        if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
            println!(
                "{{\"phase\":\"gpui_device_mode\",\"mode\":\"{}\"}}",
                if legacy_probe {
                    "legacy_probe"
                } else {
                    "reuse"
                }
            );
        }
        let debug_layer_available = check_debug_layer_available();
        let factory_started = Instant::now();
        let dxgi_factory =
            get_dxgi_factory(debug_layer_available).context("Creating DXGI factory")?;
        trace_stage("gpui_dxgi_factory", factory_started);
        let selection_started = Instant::now();
        let selected = if legacy_probe {
            // 保持发布版本的探测/释放/正式创建路径，供同一个 Release 做 A/B。
            let adapter = get_adapter(&dxgi_factory, debug_layer_available)
                .context("Getting DXGI adapter")?;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut feature_level = D3D_FEATURE_LEVEL::default();
            let device = get_device(
                &adapter,
                Some(&mut context),
                Some(&mut feature_level),
                debug_layer_available,
            )
            .context("Creating Direct3D device")?;
            AdapterDevice {
                adapter,
                device,
                device_context: context.unwrap(),
                feature_level,
            }
        } else {
            get_adapter_device(&dxgi_factory, debug_layer_available)
                .context("Getting DXGI adapter")?
        };
        trace_stage("gpui_adapter_selection", selection_started);
        let AdapterDevice {
            adapter,
            device,
            device_context,
            feature_level,
        } = selected;
        match feature_level {
            D3D_FEATURE_LEVEL_11_1 => {
                log::info!("Created device with Direct3D 11.1 feature level.")
            }
            D3D_FEATURE_LEVEL_11_0 => {
                log::info!("Created device with Direct3D 11.0 feature level.")
            }
            D3D_FEATURE_LEVEL_10_1 => {
                log::info!("Created device with Direct3D 10.1 feature level.")
            }
            _ => unreachable!(),
        }
        trace_stage("gpui_directx_init", started);

        Ok(Self {
            adapter,
            dxgi_factory,
            device,
            device_context,
        })
    }
}

struct AdapterDevice {
    adapter: IDXGIAdapter1,
    device: ID3D11Device,
    device_context: ID3D11DeviceContext,
    feature_level: D3D_FEATURE_LEVEL,
}

/// 与旧路径按相同顺序选适配器，但保留 get_device 已创建并验证的 device/context。
fn get_adapter_device(
    dxgi_factory: &IDXGIFactory6,
    debug_layer_available: bool,
) -> Result<AdapterDevice> {
    for adapter_index in 0.. {
        let adapter = enum_adapter(dxgi_factory, adapter_index)?;
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let gpu_name = String::from_utf16_lossy(&desc.Description)
                .trim_matches(char::from(0))
                .to_string();
            log::info!("Using GPU: {}", gpu_name);
            if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
                println!("{{\"phase\":\"gpui_adapter\",\"index\":{adapter_index},\"name\":{gpu_name:?}}}");
            }
        }
        let mut device_context: Option<ID3D11DeviceContext> = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        if let Some(device) = get_device(
            &adapter,
            Some(&mut device_context),
            Some(&mut feature_level),
            debug_layer_available,
        )
        .log_err()
        {
            return Ok(AdapterDevice {
                adapter,
                device,
                device_context: device_context.unwrap(),
                feature_level,
            });
        }
    }
    unreachable!()
}

fn trace_stage(stage: &str, started: Instant) {
    if std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some() {
        println!(
            "{{\"phase\":\"startup_stage\",\"stage\":\"{stage}\",\"duration_ms\":{:.3}}}",
            started.elapsed().as_secs_f64() * 1000.
        );
    }
}

#[inline]
fn check_debug_layer_available() -> bool {
    #[cfg(debug_assertions)]
    {
        use windows::Win32::Graphics::Dxgi::{DXGIGetDebugInterface1, IDXGIInfoQueue};

        unsafe { DXGIGetDebugInterface1::<IDXGIInfoQueue>(0) }
            .log_err()
            .is_some()
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

#[inline]
fn get_dxgi_factory(debug_layer_available: bool) -> Result<IDXGIFactory6> {
    let factory_flag = if debug_layer_available {
        DXGI_CREATE_FACTORY_DEBUG
    } else {
        #[cfg(debug_assertions)]
        log::warn!(
            "Failed to get DXGI debug interface. DirectX debugging features will be disabled."
        );
        DXGI_CREATE_FACTORY_FLAGS::default()
    };
    unsafe { Ok(CreateDXGIFactory2(factory_flag)?) }
}

/// RustShot QA：`RUSTSHOT_GPUI_GPU_PREFERENCE=minimum_power` 按省电顺序枚举（混合显卡时核显在前），
/// 用于对照设备创建耗时；默认保持原 `EnumAdapters` 顺序。
fn enum_adapter(dxgi_factory: &IDXGIFactory6, index: u32) -> Result<IDXGIAdapter1> {
    if std::env::var("RUSTSHOT_GPUI_GPU_PREFERENCE").as_deref() == Ok("minimum_power") {
        Ok(unsafe {
            dxgi_factory.EnumAdapterByGpuPreference(index, DXGI_GPU_PREFERENCE_MINIMUM_POWER)?
        })
    } else {
        Ok(unsafe { dxgi_factory.EnumAdapters(index)?.cast()? })
    }
}

#[inline]
fn get_adapter(dxgi_factory: &IDXGIFactory6, debug_layer_available: bool) -> Result<IDXGIAdapter1> {
    for adapter_index in 0.. {
        let adapter: IDXGIAdapter1 = unsafe { dxgi_factory.EnumAdapters(adapter_index)?.cast()? };
        if let Ok(desc) = unsafe { adapter.GetDesc1() } {
            let gpu_name = String::from_utf16_lossy(&desc.Description)
                .trim_matches(char::from(0))
                .to_string();
            log::info!("Using GPU: {}", gpu_name);
        }
        // Check to see whether the adapter supports Direct3D 11, but don't
        // create the actual device yet.
        if get_device(&adapter, None, None, debug_layer_available)
            .log_err()
            .is_some()
        {
            return Ok(adapter);
        }
    }

    unreachable!()
}

#[inline]
fn get_device(
    adapter: &IDXGIAdapter1,
    context: Option<*mut Option<ID3D11DeviceContext>>,
    feature_level: Option<*mut D3D_FEATURE_LEVEL>,
    debug_layer_available: bool,
) -> Result<ID3D11Device> {
    let probe = context.is_none();
    let mut device: Option<ID3D11Device> = None;
    let device_flags = if debug_layer_available {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG
    } else {
        D3D11_CREATE_DEVICE_BGRA_SUPPORT
    };
    let creation_started = Instant::now();
    let result = unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            device_flags,
            // 4x MSAA is required for Direct3D Feature Level 10.1 or better
            Some(&[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            feature_level,
            context,
        )
    };
    trace_stage(
        if probe {
            "gpui_d3d11_probe_create"
        } else {
            "gpui_d3d11_device_create"
        },
        creation_started,
    );
    result?;
    let device = device.unwrap();
    let mut data = D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS::default();
    let validation_started = Instant::now();
    let result = unsafe {
        device
            .CheckFeatureSupport(
                D3D11_FEATURE_D3D10_X_HARDWARE_OPTIONS,
                &mut data as *mut _ as _,
                std::mem::size_of::<D3D11_FEATURE_DATA_D3D10_X_HARDWARE_OPTIONS>() as u32,
            )
            .context("Checking GPU device feature support")
    };
    trace_stage(
        if probe {
            "gpui_d3d11_probe_validation"
        } else {
            "gpui_d3d11_device_validation"
        },
        validation_started,
    );
    result?;
    if data
        .ComputeShaders_Plus_RawAndStructuredBuffers_Via_Shader_4_x
        .as_bool()
    {
        Ok(device)
    } else {
        Err(anyhow::anyhow!(
            "Required feature StructuredBuffer is not supported by GPU/driver"
        ))
    }
}
