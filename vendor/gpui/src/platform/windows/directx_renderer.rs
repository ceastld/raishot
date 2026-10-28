use std::{
    mem::ManuallyDrop,
    sync::{
        Arc, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use ::util::ResultExt;
use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::HWND,
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            DirectComposition::*,
            DirectWrite::*,
            Dxgi::{Common::*, *},
        },
    },
    core::Interface,
};

use crate::{
    platform::windows::directx_renderer::shader_resources::{
        RawShaderBytes, ShaderModule, ShaderTarget,
    },
    *,
};

pub(crate) const DISABLE_DIRECT_COMPOSITION: &str = "GPUI_DISABLE_DIRECT_COMPOSITION";
const RENDER_TARGET_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;
// This configuration is used for MSAA rendering on paths only, and it's guaranteed to be supported by DirectX 11.
const PATH_MULTISAMPLE_COUNT: u32 = 4;
// 所有同时存活的组色目标及组内 MSAA/resolve 按真实样本 bytes 累计。
// 既有 atlas/主窗口不计入此临时额度；路径使用可见 ROI，必要时分块以保留合法多屏范围。
const ROUNDED_CONTENT_MAX_BYTES: u64 = 512 * 1024 * 1024;
const ROUNDED_PATH_MAX_TILES: u64 = 256;
const ROUNDED_CONTENT_MAX_EDGE: u32 = 16_384;
const ROUNDED_CONTENT_MAX_DEPTH: usize = 8;
// 主窗口路径批次按边界使用局部中间面；清空与 resolve 的成本随笔迹大小而非窗口大小。
// 尺寸按 2 的幂分档复用，接近整窗时仍回到整窗中间面。
const PATH_TILE_MIN_EDGE: u32 = 256;
const PATH_TILE_CACHE: usize = 4;

pub(crate) struct FontInfo {
    pub gamma_ratios: [f32; 4],
    pub grayscale_enhanced_contrast: f32,
}

pub(crate) struct DirectXRenderer {
    hwnd: HWND,
    atlas: Arc<DirectXAtlas>,
    devices: ManuallyDrop<DirectXRendererDevices>,
    resources: ManuallyDrop<DirectXResources>,
    globals: DirectXGlobalElements,
    pipelines: DirectXRenderPipelines,
    direct_composition: Option<DirectComposition>,
    font_info: &'static FontInfo,
    eager_paths: bool,
    trace_startup: bool,
    first_draw: bool,
    /// 下一次绘制只渲染、不 Present；`present_pending` 记录已渲染待提交的帧。
    defer_next_present: bool,
    present_pending: bool,
    disable_direct_composition: bool,
    resource_recovery_probe: bool,
    // 一次性读取本窗口的 GPU 帧缓冲；普通绘制不创建读回纹理。
    frame_capture_request: Option<[u32; 4]>,
    frame_capture_result: Option<Result<Vec<u8>>>,
    rounded_content_targets: Vec<RoundedContentTarget>,
    rounded_content_bytes: u64,
    frame_content_completions: Vec<Arc<AtomicU64>>,
    path_tiles: Vec<PathTileResources>,
    active_path_tile: Option<ActivePathTile>,
    retained_layers: Vec<RetainedEntry>,
}

/// 跨帧保留层的纹理。`key` 只在整组绘制完成后登记；绘制中途失败时为 0，不能复用。
struct RetainedEntry {
    slot: Weak<RetainedLayer>,
    layout: RoundedTargetLayout,
    texture: ID3D11Texture2D,
    view: [Option<ID3D11RenderTargetView>; 1],
    srv: [Option<ID3D11ShaderResourceView>; 1],
    key: u64,
    /// 产生当前内容的那次 paint；同一 Scene 重复 Present 时据此跳过重绘，追加不会叠两次。
    content_id: u64,
    used: bool,
}

impl RetainedEntry {
    fn belongs_to(&self, layer: &Arc<RetainedLayer>) -> bool {
        std::ptr::eq(self.slot.as_ptr(), Arc::as_ptr(layer))
    }

    fn publish(&self, key: u64) {
        if let Some(slot) = self.slot.upgrade() {
            slot.ready.store(key, Ordering::Release);
        }
    }

    fn invalidate(&mut self) {
        self.key = 0;
        self.content_id = 0;
        self.publish(0);
    }
}

#[derive(Clone, Copy)]
struct RetainedTarget {
    index: usize,
    key: u64,
    id: u64,
    /// false 为复用：内部原语已跳过，结束时只合成，不改动登记。
    drawing: bool,
}

struct PathTileResources {
    size: [u32; 2],
    resources: PathResources,
}

#[derive(Clone, Copy)]
struct ActivePathTile {
    index: usize,
    layout: RoundedTargetLayout,
    clip: RoundedTargetLayout,
}

/// 局部路径中间面的分档尺寸；`None` 表示应直接使用整窗中间面。
fn path_tile_size(needed: [u32; 2], main: [u32; 2]) -> Option<[u32; 2]> {
    let edge = |n: u32, max: u32| n.max(PATH_TILE_MIN_EDGE).next_power_of_two().min(max);
    let size = [edge(needed[0], main[0]), edge(needed[1], main[1])];
    let main_pixels = u64::from(main[0]) * u64::from(main[1]);
    (u64::from(size[0]) * u64::from(size[1]) * 2 < main_pixels).then_some(size)
}

/// Direct3D objects
#[derive(Clone)]
pub(crate) struct DirectXRendererDevices {
    pub(crate) adapter: IDXGIAdapter1,
    pub(crate) dxgi_factory: IDXGIFactory6,
    pub(crate) device: ID3D11Device,
    pub(crate) device_context: ID3D11DeviceContext,
    dxgi_device: Option<IDXGIDevice>,
}

struct DirectXResources {
    // Direct3D rendering objects
    swap_chain: Option<IDXGISwapChain1>,
    render_target: Option<ID3D11Texture2D>,
    render_target_view: [Option<ID3D11RenderTargetView>; 1],

    // 首个非空 paths 批次才创建，后续同尺寸绘制复用，resize/device-lost 后失效。
    paths: Option<PathResources>,

    // Cached window size and viewport
    width: u32,
    height: u32,
    viewport: [D3D11_VIEWPORT; 1],
}

struct PathResources {
    intermediate_texture: ID3D11Texture2D,
    intermediate_srv: [Option<ID3D11ShaderResourceView>; 1],
    intermediate_view: [Option<ID3D11RenderTargetView>; 1],
    intermediate_msaa_texture: ID3D11Texture2D,
    intermediate_msaa_view: [Option<ID3D11RenderTargetView>; 1],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RoundedTargetLayout {
    origin: [u32; 2],
    size: [u32; 2],
}

impl RoundedTargetLayout {
    fn pixels(self) -> u64 {
        u64::from(self.size[0]) * u64::from(self.size[1])
    }

    fn viewport(self) -> [D3D11_VIEWPORT; 1] {
        [D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.size[0] as f32,
            Height: self.size[1] as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]
    }

    fn bounds(self) -> Bounds<ScaledPixels> {
        Bounds {
            origin: point(
                ScaledPixels(self.origin[0] as f32),
                ScaledPixels(self.origin[1] as f32),
            ),
            size: size(
                ScaledPixels(self.size[0] as f32),
                ScaledPixels(self.size[1] as f32),
            ),
        }
    }
}

struct RoundedContentResources {
    // 所有组资源仅活到当前帧的 End；组失败/resize/恢复也先解绑再 Drop。
    _texture: ID3D11Texture2D,
    view: [Option<ID3D11RenderTargetView>; 1],
    srv: [Option<ID3D11ShaderResourceView>; 1],
    paths: Option<RoundedPathResources>,
    mask: Option<RoundedMaskResources>,
}

struct RoundedMaskResources {
    _texture: ID3D11Texture2D,
    srv: [Option<ID3D11ShaderResourceView>; 1],
    bytes: u64,
}

struct RoundedPathResources {
    layout: RoundedTargetLayout,
    clip_layout: RoundedTargetLayout,
    resources: PathResources,
    bytes: u64,
}

struct RoundedContentTarget {
    group: RoundedContentGroup,
    layout: Option<RoundedTargetLayout>,
    resources: Option<RoundedContentResources>,
    charged_bytes: u64,
    retained: Option<RetainedTarget>,
}

fn rounded_target_layout(
    group: &RoundedContentGroup,
    parent: Option<RoundedTargetLayout>,
) -> Result<Option<RoundedTargetLayout>> {
    let b = group.bounds;
    let c = group.clip_bounds;
    let values = [
        b.origin.x.0,
        b.origin.y.0,
        b.size.width.0,
        b.size.height.0,
        c.origin.x.0,
        c.origin.y.0,
        c.size.width.0,
        c.size.height.0,
        group.corner_radii.top_left.0,
        group.corner_radii.top_right.0,
        group.corner_radii.bottom_right.0,
        group.corner_radii.bottom_left.0,
        group.corner_radii_y.top_left.0,
        group.corner_radii_y.top_right.0,
        group.corner_radii_y.bottom_right.0,
        group.corner_radii_y.bottom_left.0,
    ];
    anyhow::ensure!(
        values.iter().all(|v| v.is_finite()),
        "Rounded content geometry is not finite"
    );
    anyhow::ensure!(
        values[2] >= 0.0
            && values[3] >= 0.0
            && values[6] >= 0.0
            && values[7] >= 0.0
            && values[8..].iter().all(|v| *v >= 0.0),
        "Rounded content size or radius is negative"
    );
    let ends = [
        values[0] + values[2],
        values[1] + values[3],
        values[4] + values[6],
        values[5] + values[7],
    ];
    anyhow::ensure!(
        ends.iter().all(|v| v.is_finite()),
        "Rounded content bounds overflow"
    );
    let Some(parent) = parent else {
        return Ok(None);
    };
    let left = values[0].max(values[4]).max(parent.origin[0] as f32);
    let top = values[1].max(values[5]).max(parent.origin[1] as f32);
    let right = ends[0]
        .min(ends[2])
        .min((parent.origin[0] + parent.size[0]) as f32);
    let bottom = ends[1]
        .min(ends[3])
        .min((parent.origin[1] + parent.size[1]) as f32);
    if left >= right || top >= bottom {
        return Ok(None);
    }
    // 只在此对可见 ROI 向外取整；原语/轮廓/clip 保留原分数坐标和物理像素相位。
    let origin = [left.floor() as u32, top.floor() as u32];
    let size = [
        right.ceil() as u32 - origin[0],
        bottom.ceil() as u32 - origin[1],
    ];
    anyhow::ensure!(
        size.iter().all(|v| *v <= ROUNDED_CONTENT_MAX_EDGE),
        "Rounded content target exceeds the texture limit"
    );
    Ok(Some(RoundedTargetLayout { origin, size }))
}

fn rounded_resource_charge(current: u64, pixels: u64, samples: u64) -> Result<u64> {
    let charge = pixels
        .checked_mul(samples)
        .and_then(|v| v.checked_mul(4))
        .context("Rounded content resource byte charge overflow")?;
    let total = current
        .checked_add(charge)
        .context("Rounded content resource byte budget overflow")?;
    anyhow::ensure!(
        total <= ROUNDED_CONTENT_MAX_BYTES,
        "Rounded content GPU byte budget exceeded"
    );
    Ok(charge)
}

// 每块只负责一组整数像素，复合矩形不重叠，AA/原语坐标不在块边界重置。
fn rounded_path_tile_layouts(
    layout: RoundedTargetLayout,
    available_bytes: u64,
) -> Result<Vec<RoundedTargetLayout>> {
    anyhow::ensure!(
        layout.size.iter().all(|v| *v != 0),
        "Rounded path tile region is empty"
    );
    let samples_per_pixel = 1 + u64::from(PATH_MULTISAMPLE_COUNT);
    let max_pixels = available_bytes / (4 * samples_per_pixel);
    anyhow::ensure!(
        max_pixels != 0,
        "Rounded content GPU byte budget has no path space"
    );
    let width = max_pixels.min(u64::from(layout.size[0])) as u32;
    let height = (max_pixels / u64::from(width)).min(u64::from(layout.size[1])) as u32;
    let count =
        u64::from(layout.size[0].div_ceil(width)) * u64::from(layout.size[1].div_ceil(height));
    anyhow::ensure!(
        count <= ROUNDED_PATH_MAX_TILES,
        "Rounded content path tiling limit exceeded"
    );
    let mut tiles = Vec::new();
    tiles
        .try_reserve_exact(count as usize)
        .context("Allocating rounded path tile layouts")?;
    let mut y = 0;
    while y < layout.size[1] {
        let h = height.min(layout.size[1] - y);
        let mut x = 0;
        while x < layout.size[0] {
            let w = width.min(layout.size[0] - x);
            tiles.push(RoundedTargetLayout {
                origin: [layout.origin[0] + x, layout.origin[1] + y],
                size: [w, h],
            });
            x += w;
        }
        y += h;
    }
    Ok(tiles)
}

fn validate_content_group_ready(
    group: &RoundedContentGroup,
    layout: Option<RoundedTargetLayout>,
) -> Result<()> {
    anyhow::ensure!(
        group.content_ready,
        "Rounded content CPU preparation or painting failed"
    );
    anyhow::ensure!(
        group.completion.is_none() || layout.is_some(),
        "Tracked content group has no visible content"
    );
    Ok(())
}

fn validate_content_mask_path(path: &Path<ScaledPixels>) -> Result<()> {
    let b = path.bounds;
    let c = path.content_mask.bounds;
    let geometry = [
        b.origin.x.0,
        b.origin.y.0,
        b.size.width.0,
        b.size.height.0,
        c.origin.x.0,
        c.origin.y.0,
        c.size.width.0,
        c.size.height.0,
        b.origin.x.0 + b.size.width.0,
        b.origin.y.0 + b.size.height.0,
        c.origin.x.0 + c.size.width.0,
        c.origin.y.0 + c.size.height.0,
    ];
    anyhow::ensure!(
        geometry.iter().all(|v| v.is_finite()),
        "Content mask geometry is not finite"
    );
    anyhow::ensure!(
        geometry[2] >= 0.0 && geometry[3] >= 0.0 && geometry[6] >= 0.0 && geometry[7] >= 0.0,
        "Content mask size is negative"
    );
    anyhow::ensure!(
        !path.vertices.is_empty() && path.vertices.len() % 3 == 0,
        "Content mask is not a complete tessellated path"
    );
    anyhow::ensure!(
        u32::try_from(path.vertices.len()).is_ok(),
        "Content mask has too many vertices"
    );
    anyhow::ensure!(
        path.vertices.iter().all(|v| [
            v.xy_position.x.0,
            v.xy_position.y.0,
            v.st_position.x,
            v.st_position.y
        ]
        .iter()
        .all(|f| f.is_finite())),
        "Content mask vertex is not finite"
    );
    Ok(())
}

fn rounded_mask_tile_layouts(
    layout: RoundedTargetLayout,
    base_bytes: u64,
) -> Result<(u64, Vec<RoundedTargetLayout>)> {
    let mask_bytes = rounded_resource_charge(base_bytes, layout.pixels(), 1)?;
    let available = ROUNDED_CONTENT_MAX_BYTES - (base_bytes + mask_bytes);
    Ok((mask_bytes, rounded_path_tile_layouts(layout, available)?))
}

fn reset_scene_content_completions(scene: &Scene) {
    // Cached Scene replay may skip Window/Canvas paint. Reset every Begin before
    // pre_draw, resource recovery or any GPU operation can fail ahead of Start.
    for operation in &scene.paint_operations {
        if let PaintOperation::StartRoundedContentGroup(group) = operation {
            if let Some(completion) = &group.completion {
                completion.store(0, Ordering::Release);
            }
        }
    }
}

fn publish_content_completions(
    completions: &mut Vec<Arc<AtomicU64>>,
    size: [u32; 2],
    presented: bool,
) {
    let marker = if presented && size[0] != 0 && size[1] != 0 {
        (u64::from(size[0]) << 32) | u64::from(size[1])
    } else {
        0
    };
    for completion in completions.drain(..) {
        completion.store(marker, Ordering::Release);
    }
}

struct MappedFrameCapture<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
}

impl Drop for MappedFrameCapture<'_> {
    fn drop(&mut self) {
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

struct DirectXRenderPipelines {
    shadow_pipeline: PipelineState<Shadow>,
    quad_pipeline: PipelineState<Quad>,
    path_rasterization_pipeline: PipelineState<PathRasterizationSprite>,
    path_union_blend_state: ID3D11BlendState,
    path_sprite_pipeline: PipelineState<PathSprite>,
    underline_pipeline: PipelineState<Underline>,
    mono_sprites: PipelineState<MonochromeSprite>,
    poly_sprites: PipelineState<PolychromeSprite>,
    grouped_poly_sprites: PipelineState<PolychromeSprite>,
    rounded_content: PipelineState<RoundedContentSprite>,
    group_straight_blend: ID3D11BlendState,
    group_premultiplied_blend: ID3D11BlendState,
}

struct DirectXGlobalElements {
    global_params_buffer: [Option<ID3D11Buffer>; 1],
    sampler: [Option<ID3D11SamplerState>; 1],
}

struct DirectComposition {
    comp_device: IDCompositionDevice,
    comp_target: IDCompositionTarget,
    comp_visual: IDCompositionVisual,
}

impl DirectXRendererDevices {
    pub(crate) fn new(
        directx_devices: &DirectXDevices,
        disable_direct_composition: bool,
    ) -> Result<Self> {
        let DirectXDevices {
            adapter,
            dxgi_factory,
            device,
            device_context,
        } = directx_devices;
        let dxgi_device = if disable_direct_composition {
            None
        } else {
            Some(device.cast().context("Creating DXGI device")?)
        };

        Ok(Self {
            adapter: adapter.clone(),
            dxgi_factory: dxgi_factory.clone(),
            device: device.clone(),
            device_context: device_context.clone(),
            dxgi_device,
        })
    }
}

impl DirectXRenderer {
    pub(crate) fn new(
        hwnd: HWND,
        directx_devices: &DirectXDevices,
        disable_direct_composition: bool,
    ) -> Result<Self> {
        let trace_startup = std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some();
        let eager_paths = std::env::var("RUSTSHOT_GPUI_EAGER_PATHS").as_deref() == Ok("1");
        let renderer_started = trace_startup.then(Instant::now);
        if disable_direct_composition {
            log::info!("Direct Composition is disabled.");
        }

        let devices = DirectXRendererDevices::new(directx_devices, disable_direct_composition)
            .context("Creating DirectX devices")?;
        let atlas = Arc::new(DirectXAtlas::new(&devices.device, &devices.device_context));

        let resources_started = trace_startup.then(Instant::now);
        let resources = DirectXResources::new(
            &devices,
            1,
            1,
            hwnd,
            disable_direct_composition,
            eager_paths,
            trace_startup,
        )
        .context("Creating DirectX resources")?;
        trace_renderer_stage("gpui_renderer_resources", resources_started, 1, 1);
        let globals = DirectXGlobalElements::new(&devices.device)
            .context("Creating DirectX global elements")?;
        let pipelines_started = trace_startup.then(Instant::now);
        let pipelines = DirectXRenderPipelines::new(&devices.device)
            .context("Creating DirectX render pipelines")?;
        trace_renderer_stage("gpui_renderer_pipelines", pipelines_started, 1, 1);

        let composition_started = trace_startup.then(Instant::now);
        let direct_composition = if disable_direct_composition {
            None
        } else {
            let composition = DirectComposition::new(devices.dxgi_device.as_ref().unwrap(), hwnd)
                .context("Creating DirectComposition")?;
            composition
                .set_swap_chain(
                    resources
                        .swap_chain
                        .as_ref()
                        .context("DirectX swap chain unavailable")?,
                )
                .context("Setting swap chain for DirectComposition")?;
            Some(composition)
        };
        trace_renderer_stage("gpui_renderer_composition", composition_started, 1, 1);
        let font_started = trace_startup.then(Instant::now);
        let font_info = Self::get_font_info();
        trace_renderer_stage("gpui_renderer_font_info", font_started, 1, 1);
        trace_renderer_stage("gpui_renderer_new", renderer_started, 1, 1);
        if trace_startup {
            println!(
                "{{\"phase\":\"gpui_path_mode\",\"mode\":\"{}\"}}",
                if eager_paths { "eager" } else { "lazy" }
            );
        }

        Ok(DirectXRenderer {
            hwnd,
            atlas,
            devices: ManuallyDrop::new(devices),
            resources: ManuallyDrop::new(resources),
            globals,
            pipelines,
            direct_composition,
            font_info,
            eager_paths,
            trace_startup,
            first_draw: true,
            defer_next_present: false,
            present_pending: false,
            disable_direct_composition,
            resource_recovery_probe: std::env::var("RUSTSHOT_GPUI_RESOURCE_RECOVERY_PROBE")
                .as_deref()
                == Ok("1"),
            frame_capture_request: None,
            frame_capture_result: None,
            rounded_content_targets: Vec::new(),
            rounded_content_bytes: 0,
            frame_content_completions: Vec::new(),
            path_tiles: Vec::new(),
            active_path_tile: None,
            retained_layers: Vec::new(),
        })
    }

    pub(crate) fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.atlas.clone()
    }

    fn main_target_layout(&self) -> RoundedTargetLayout {
        RoundedTargetLayout {
            origin: [0, 0],
            size: [self.resources.width, self.resources.height],
        }
    }

    fn active_target_layout(&self) -> Option<RoundedTargetLayout> {
        self.rounded_content_targets
            .last()
            .map_or(Some(self.main_target_layout()), |t| t.layout)
    }

    fn active_viewport(&self) -> [D3D11_VIEWPORT; 1] {
        self.active_target_layout()
            .map_or(self.resources.viewport, RoundedTargetLayout::viewport)
    }

    fn bind_active_target(&self) -> Result<()> {
        let Some(layout) = self.active_target_layout() else {
            return Ok(());
        };
        let view = if let Some(group) = self.rounded_content_targets.last() {
            &group
                .resources
                .as_ref()
                .context("Rounded content target is unavailable")?
                .view
        } else {
            &self.resources.render_target_view
        };
        anyhow::ensure!(
            view[0].is_some(),
            "DirectX render target is unavailable after resize"
        );
        self.set_target_globals(layout)?;
        unsafe {
            self.devices
                .device_context
                .OMSetRenderTargets(Some(view), None);
            self.devices
                .device_context
                .RSSetViewports(Some(&layout.viewport()));
            self.devices
                .device_context
                .VSSetConstantBuffers(0, Some(&self.globals.global_params_buffer));
            self.devices
                .device_context
                .PSSetConstantBuffers(0, Some(&self.globals.global_params_buffer));
        }
        Ok(())
    }

    fn set_target_globals(&self, layout: RoundedTargetLayout) -> Result<()> {
        update_buffer(
            &self.devices.device_context,
            self.globals.global_params_buffer[0].as_ref().unwrap(),
            &[GlobalParams {
                gamma_ratios: self.font_info.gamma_ratios,
                viewport_size: [layout.size[0] as f32, layout.size[1] as f32],
                grayscale_enhanced_contrast: self.font_info.grayscale_enhanced_contrast,
                rounded_content: u32::from(!self.rounded_content_targets.is_empty()),
                render_target_origin: [layout.origin[0] as f32, layout.origin[1] as f32],
                _pad: [0; 2],
            }],
        )
    }

    fn unbind_content_resources(&self) {
        unsafe {
            self.devices.device_context.OMSetRenderTargets(None, None);
            self.devices
                .device_context
                .VSSetShaderResources(0, Some(&[None, None, None]));
            self.devices
                .device_context
                .PSSetShaderResources(0, Some(&[None, None, None]));
        }
    }

    fn discard_rounded_content_targets(&mut self) {
        self.unbind_content_resources();
        self.rounded_content_targets.clear();
        self.rounded_content_bytes = 0;
    }

    fn pre_draw(&self) -> Result<()> {
        self.bind_active_target()?;
        let render_target_view = self.resources.render_target_view[0].as_ref().unwrap();
        unsafe {
            self.devices
                .device_context
                .ClearRenderTargetView(render_target_view, &[0.0; 4]);
        }
        Ok(())
    }

    /// 返回 true 表示保留层直接复用，调用方跳过组内原语直到对应 End。
    fn start_rounded_content_group(&mut self, group: RoundedContentGroup) -> Result<bool> {
        anyhow::ensure!(
            self.rounded_content_targets.len() < ROUNDED_CONTENT_MAX_DEPTH,
            "Rounded content nesting limit exceeded"
        );
        if let Some(completion) = &group.completion {
            completion.store(0, Ordering::Release);
        }
        let layout = rounded_target_layout(&group, self.active_target_layout())?;
        validate_content_group_ready(&group, layout)?;
        if let Some(mask) = &group.mask {
            validate_content_mask_path(mask)?;
        }
        if let Some(completion) = &group.completion {
            self.frame_content_completions
                .try_reserve(1)
                .context("Allocating content completion list")?;
            self.frame_content_completions.push(completion.clone());
        }
        if layout.is_some() {
            self.retire_rounded_paths();
        }
        if group.retained.is_some() {
            return self.start_retained_layer(group, layout);
        }
        let mut charged_bytes = 0;
        let resources = if let Some(layout) = layout {
            charged_bytes =
                rounded_resource_charge(self.rounded_content_bytes, layout.pixels(), 1)?;
            let (texture, srv) = create_path_intermediate_texture(
                &self.devices.device,
                layout.size[0],
                layout.size[1],
            )?;
            let mut view = None;
            unsafe {
                self.devices
                    .device
                    .CreateRenderTargetView(&texture, None, Some(&mut view))?;
            }
            let view = [view];
            unsafe {
                self.devices
                    .device_context
                    .ClearRenderTargetView(view[0].as_ref().unwrap(), &[0.0; 4]);
            }
            let mask = if let Some(mask) = &group.mask {
                Some(self.prepare_rounded_content_mask(
                    mask,
                    layout,
                    self.rounded_content_bytes + charged_bytes,
                )?)
            } else {
                None
            };
            if let Some(mask) = &mask {
                charged_bytes += mask.bytes;
            }
            Some(RoundedContentResources {
                _texture: texture,
                view,
                srv,
                paths: None,
                mask,
            })
        } else {
            None
        };
        self.unbind_content_resources();
        self.rounded_content_targets.push(RoundedContentTarget {
            group,
            layout,
            resources,
            charged_bytes,
            retained: None,
        });
        self.rounded_content_bytes += charged_bytes;
        self.bind_active_target()?;
        Ok(false)
    }

    fn start_retained_layer(
        &mut self,
        group: RoundedContentGroup,
        layout: Option<RoundedTargetLayout>,
    ) -> Result<bool> {
        let op = group
            .retained
            .clone()
            .context("Missing retained layer request")?;
        let Some(layout) = layout else {
            // 不可见：已有纹理与登记原样保留，组内原语没有目标可画。
            self.rounded_content_targets.push(RoundedContentTarget {
                group,
                layout: None,
                resources: None,
                charged_bytes: 0,
                retained: None,
            });
            return Ok(true);
        };
        let found = self
            .retained_layers
            .iter()
            .position(|entry| entry.belongs_to(&op.layer));
        let current = found
            .map(|index| &self.retained_layers[index])
            .filter(|entry| entry.layout == layout);
        let reuse = current.is_some_and(|entry| {
            entry.content_id == op.id
                || (op.mode == RetainedPaint::Reuse && op.key != 0 && entry.key == op.key)
        });
        let append = !reuse
            && matches!(op.mode, RetainedPaint::Append { base }
                if base != 0 && current.is_some_and(|entry| entry.key == base));
        if !(reuse || append || op.mode == RetainedPaint::Rebuild) {
            // 窗口或父组 ROI 变化使旧纹理失效：本帧放弃，强制整帧重绘让上层改为重建，
            // 不能只靠重复 Present 同一 Scene（它会一直缺这层内容）。
            op.layer.ready.store(0, Ordering::Release);
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(self.hwnd),
                    crate::platform::windows::WM_GPUI_FORCE_UPDATE_WINDOW,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                )
                .log_err();
            }
            anyhow::bail!("Retained layer content is no longer available");
        }
        let charged_bytes =
            rounded_resource_charge(self.rounded_content_bytes, layout.pixels(), 1)?;
        let index = match found {
            Some(index) if self.retained_layers[index].layout.size == layout.size => index,
            found => {
                let (texture, srv) = create_path_intermediate_texture(
                    &self.devices.device,
                    layout.size[0],
                    layout.size[1],
                )?;
                let mut view = None;
                unsafe {
                    self.devices
                        .device
                        .CreateRenderTargetView(&texture, None, Some(&mut view))?;
                }
                let entry = RetainedEntry {
                    slot: Arc::downgrade(&op.layer),
                    layout,
                    texture,
                    view: [view],
                    srv,
                    key: 0,
                    content_id: 0,
                    used: true,
                };
                self.unbind_content_resources();
                if let Some(index) = found {
                    self.retained_layers[index] = entry;
                    index
                } else {
                    self.retained_layers.push(entry);
                    self.retained_layers.len() - 1
                }
            }
        };
        let entry = &mut self.retained_layers[index];
        entry.used = true;
        if !reuse {
            // 原点可随重建变化；绘制完成前不登记任何可复用内容。
            entry.layout = layout;
            entry.invalidate();
            if !append {
                unsafe {
                    self.devices
                        .device_context
                        .ClearRenderTargetView(entry.view[0].as_ref().unwrap(), &[0.0; 4]);
                }
            }
        }
        let resources = RoundedContentResources {
            _texture: entry.texture.clone(),
            view: entry.view.clone(),
            srv: entry.srv.clone(),
            paths: None,
            mask: None,
        };
        self.unbind_content_resources();
        self.rounded_content_targets.push(RoundedContentTarget {
            group,
            layout: Some(layout),
            resources: Some(resources),
            charged_bytes,
            retained: Some(RetainedTarget {
                index,
                key: op.key,
                id: op.id,
                drawing: !reuse,
            }),
        });
        self.rounded_content_bytes += charged_bytes;
        self.bind_active_target()?;
        Ok(reuse)
    }

    /// 释放本帧未使用的保留层；resize、设备恢复时整体清空。
    fn sweep_retained_layers(&mut self, all: bool) {
        if !all && self.retained_layers.iter().all(|entry| entry.used) {
            for entry in &mut self.retained_layers {
                entry.used = false;
            }
            return;
        }
        self.unbind_content_resources();
        self.retained_layers.retain_mut(|entry| {
            let keep = entry.used && !all && entry.slot.strong_count() > 0;
            if !keep {
                entry.invalidate();
            }
            entry.used = false;
            keep
        });
    }

    fn prepare_rounded_content_mask(
        &mut self,
        mask: &Path<ScaledPixels>,
        layout: RoundedTargetLayout,
        base_bytes: u64,
    ) -> Result<RoundedMaskResources> {
        validate_content_mask_path(mask)?;
        let (mask_bytes, tiles) = rounded_mask_tile_layouts(layout, base_bytes)?;
        let (texture, srv) =
            create_path_intermediate_texture(&self.devices.device, layout.size[0], layout.size[1])?;
        let mut vertices = Vec::new();
        vertices
            .try_reserve_exact(mask.vertices.len())
            .context("Allocating content mask vertices")?;
        vertices.extend(mask.vertices.iter().map(|v| PathRasterizationSprite {
            xy_position: v.xy_position,
            st_position: v.st_position,
            color: Background::from(crate::rgba(0xffffffff)),
            bounds: mask.clipped_bounds(),
        }));
        self.pipelines.path_rasterization_pipeline.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            &vertices,
        )?;
        let mut working: Option<(RoundedTargetLayout, PathResources)> = None;
        for tile in tiles {
            self.unbind_content_resources();
            if !working
                .as_ref()
                .is_some_and(|(old, _)| old.size[0] >= tile.size[0] && old.size[1] >= tile.size[1])
            {
                drop(working.take());
                rounded_resource_charge(
                    base_bytes + mask_bytes,
                    tile.pixels(),
                    1 + u64::from(PATH_MULTISAMPLE_COUNT),
                )?;
                working = Some((
                    tile,
                    PathResources::new(
                        &self.devices.device,
                        tile.size[0],
                        tile.size[1],
                        self.trace_startup,
                    )?,
                ));
            }
            let (working_layout, resources) = working.as_mut().unwrap();
            working_layout.origin = tile.origin;
            self.set_target_globals(*working_layout)?;
            unsafe {
                self.devices.device_context.ClearRenderTargetView(
                    resources.intermediate_msaa_view[0].as_ref().unwrap(),
                    &[0.0; 4],
                );
                self.devices
                    .device_context
                    .OMSetRenderTargets(Some(&resources.intermediate_msaa_view), None);
            }
            self.pipelines.path_rasterization_pipeline.draw_with_blend(
                &self.devices.device_context,
                &working_layout.viewport(),
                &self.globals.global_params_buffer,
                D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
                vertices.len() as u32,
                1,
                &self.pipelines.path_union_blend_state,
            )?;
            self.unbind_content_resources();
            unsafe {
                self.devices.device_context.ResolveSubresource(
                    &resources.intermediate_texture,
                    0,
                    &resources.intermediate_msaa_texture,
                    0,
                    RENDER_TARGET_FORMAT,
                );
                let source_box = D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: tile.size[0],
                    bottom: tile.size[1],
                    back: 1,
                };
                self.devices.device_context.CopySubresourceRegion(
                    &texture,
                    0,
                    tile.origin[0] - layout.origin[0],
                    tile.origin[1] - layout.origin[1],
                    0,
                    &resources.intermediate_texture,
                    0,
                    Some(&source_box),
                );
            }
        }
        // 只保留完整resolve覆盖，工作MSAA及分块resolve在绘制内容前退役。
        self.unbind_content_resources();
        drop(working);
        Ok(RoundedMaskResources {
            _texture: texture,
            srv,
            bytes: mask_bytes,
        })
    }

    fn end_rounded_content_group(&mut self) -> Result<()> {
        let target = self
            .rounded_content_targets
            .pop()
            .context("Unbalanced rounded content End")?;
        self.unbind_content_resources();
        self.rounded_content_bytes -= target.charged_bytes;
        self.bind_active_target()?;
        let Some(layout) = target.layout else {
            return Ok(());
        };
        let resources = target
            .resources
            .as_ref()
            .context("Rounded content texture is unavailable")?;
        // 保留层按整数 ROI 原样拷贝，分数边缘已由组内原语自身的 content mask 处理。
        let (contour_bounds, clip_bounds) = if target.retained.is_some() {
            (layout.bounds(), layout.bounds())
        } else {
            (target.group.bounds, target.group.clip_bounds)
        };
        self.pipelines.rounded_content.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            &[RoundedContentSprite {
                bounds: layout.bounds(),
                contour_bounds,
                clip_bounds,
                corner_radii: target.group.corner_radii,
                corner_radii_y: target.group.corner_radii_y,
                has_mask: u32::from(resources.mask.is_some()),
                padding: [0; 3],
            }],
        )?;
        let mask_srv = resources
            .mask
            .as_ref()
            .map_or([None], |mask| mask.srv.clone());
        unsafe {
            self.devices
                .device_context
                .PSSetShaderResources(2, Some(&mask_srv));
        }
        let result = self.pipelines.rounded_content.draw_with_texture(
            &self.devices.device_context,
            &resources.srv,
            &self.active_viewport(),
            &self.globals.global_params_buffer,
            &self.globals.sampler,
            1,
        );
        // End 即退役，不让 SRV 持有完成的组颜色/整体mask；父目标常量仍有效。
        unsafe {
            self.devices
                .device_context
                .VSSetShaderResources(2, Some(&[None]));
            self.devices
                .device_context
                .PSSetShaderResources(2, Some(&[None]));
        }
        unsafe {
            self.devices
                .device_context
                .VSSetShaderResources(0, Some(&[None]));
            self.devices
                .device_context
                .PSSetShaderResources(0, Some(&[None]));
        }
        if let Some(retained) = target.retained.filter(|r| r.drawing && result.is_ok()) {
            let entry = &mut self.retained_layers[retained.index];
            entry.key = retained.key;
            entry.content_id = retained.id;
            entry.publish(retained.key);
        }
        result
    }

    fn retire_rounded_paths(&mut self) {
        self.unbind_content_resources();
        for target in &mut self.rounded_content_targets {
            if let Some(paths) = target.resources.as_mut().and_then(|r| r.paths.take()) {
                target.charged_bytes -= paths.bytes;
                self.rounded_content_bytes -= paths.bytes;
                drop(paths);
            }
        }
    }

    fn ensure_active_paths(&mut self, layout: Option<RoundedTargetLayout>) -> Result<()> {
        if self.rounded_content_targets.is_empty() {
            self.active_path_tile = None;
            let main = self.main_target_layout();
            if let Some(clip) = layout {
                if let Some(size) = path_tile_size(clip.size, main.size) {
                    let index = match self.path_tiles.iter().position(|t| t.size == size) {
                        Some(index) => index,
                        None => {
                            if self.path_tiles.len() >= PATH_TILE_CACHE {
                                self.unbind_content_resources();
                                self.path_tiles.remove(0);
                            }
                            self.path_tiles.push(PathTileResources {
                                size,
                                resources: PathResources::new(
                                    &self.devices.device,
                                    size[0],
                                    size[1],
                                    self.trace_startup,
                                )?,
                            });
                            self.path_tiles.len() - 1
                        }
                    };
                    self.active_path_tile = Some(ActivePathTile {
                        index,
                        layout: RoundedTargetLayout {
                            origin: clip.origin,
                            size,
                        },
                        clip,
                    });
                    return Ok(());
                }
            }
            self.resources
                .ensure_paths(&self.devices.device, self.trace_startup)?;
            return Ok(());
        }
        let layout = layout.context("Missing rounded path tile layout")?;
        // 先解除上一块 SRV/MSAA 的 context 引用，再退役，预算计实际同时存活资源。
        self.unbind_content_resources();
        let target = self.rounded_content_targets.last_mut().unwrap();
        let resources = target
            .resources
            .as_mut()
            .context("Rounded group resources unavailable")?;
        if let Some(paths) = resources.paths.as_mut() {
            if paths.layout.size[0] >= layout.size[0] && paths.layout.size[1] >= layout.size[1] {
                // 相邻 tile/较小后继复用同一 MSAA/resolve；只变绝对原点与最终复制边界。
                // Clear/Draw/Resolve 顺序由同一 immediate context 保持，不 CPU 等待或读回。
                paths.layout.origin = layout.origin;
                paths.clip_layout = layout;
                return Ok(());
            }
        }
        if let Some(old) = resources.paths.take() {
            target.charged_bytes -= old.bytes;
            self.rounded_content_bytes -= old.bytes;
            drop(old);
        }
        // 组内逐笔批次尺寸各异：按 2 的幂分档（不超过组本身）分配，后续较小批次直接复用，
        // 保留层重建几十笔时不再逐笔创建 MSAA 纹理；预算不足时退回精确尺寸。
        let group_size = target.layout.map_or(layout.size, |l| l.size);
        let bucket = |n: u32, max: u32| {
            n.max(PATH_TILE_MIN_EDGE)
                .next_power_of_two()
                .min(max.max(n))
        };
        let samples = 1 + u64::from(PATH_MULTISAMPLE_COUNT);
        let bucketed = RoundedTargetLayout {
            origin: layout.origin,
            size: [
                bucket(layout.size[0], group_size[0]),
                bucket(layout.size[1], group_size[1]),
            ],
        };
        let (path_layout, charge) =
            match rounded_resource_charge(self.rounded_content_bytes, bucketed.pixels(), samples) {
                Ok(charge) => (bucketed, charge),
                Err(_) => (
                    layout,
                    rounded_resource_charge(self.rounded_content_bytes, layout.pixels(), samples)?,
                ),
            };
        let paths = PathResources::new(
            &self.devices.device,
            path_layout.size[0],
            path_layout.size[1],
            self.trace_startup,
        )?;
        resources.paths = Some(RoundedPathResources {
            layout: path_layout,
            clip_layout: layout,
            resources: paths,
            bytes: charge,
        });
        target.charged_bytes += charge;
        self.rounded_content_bytes += charge;
        Ok(())
    }

    fn active_paths(&self) -> Result<&PathResources> {
        if let Some(target) = self.rounded_content_targets.last() {
            target
                .resources
                .as_ref()
                .and_then(|r| r.paths.as_ref())
                .map(|p| &p.resources)
                .context("Rounded group path resources unavailable")
        } else if let Some(tile) = self.active_path_tile {
            Ok(&self.path_tiles[tile.index].resources)
        } else {
            self.resources
                .paths
                .as_ref()
                .context("Path resources unavailable")
        }
    }

    fn active_path_layout(&self) -> Result<RoundedTargetLayout> {
        if let Some(target) = self.rounded_content_targets.last() {
            target
                .resources
                .as_ref()
                .and_then(|r| r.paths.as_ref())
                .map(|p| p.layout)
                .context("Rounded group path layout unavailable")
        } else if let Some(tile) = self.active_path_tile {
            Ok(tile.layout)
        } else {
            Ok(self.main_target_layout())
        }
    }

    fn active_path_clip_layout(&self) -> Result<RoundedTargetLayout> {
        if let Some(target) = self.rounded_content_targets.last() {
            target
                .resources
                .as_ref()
                .and_then(|r| r.paths.as_ref())
                .map(|p| p.clip_layout)
                .context("Rounded group path tile clip unavailable")
        } else if let Some(tile) = self.active_path_tile {
            Ok(tile.clip)
        } else {
            Ok(self.main_target_layout())
        }
    }

    fn draw_paths(&mut self, paths: &[Path<ScaledPixels>]) -> Result<()> {
        let Some(first) = paths.first() else {
            return Ok(());
        };
        if self.rounded_content_targets.is_empty() {
            // 只为本批可见边界清空/resolve，避免多笔独立批次每笔都处理整窗 MSAA。
            let main = self.main_target_layout();
            let mut bounds = first.clipped_bounds();
            for path in paths.iter().skip(1) {
                bounds = bounds.union(&path.clipped_bounds());
            }
            let bounds = bounds.intersect(&main.bounds());
            if bounds.size.width.0 <= 0. || bounds.size.height.0 <= 0. {
                return Ok(());
            }
            let left = bounds.origin.x.0.floor().max(0.) as u32;
            let top = bounds.origin.y.0.floor().max(0.) as u32;
            let right = ((bounds.origin.x.0 + bounds.size.width.0).ceil() as u32).min(main.size[0]);
            let bottom =
                ((bounds.origin.y.0 + bounds.size.height.0).ceil() as u32).min(main.size[1]);
            let tile = RoundedTargetLayout {
                origin: [left, top],
                size: [right.saturating_sub(left), bottom.saturating_sub(top)],
            };
            let result = self
                .draw_paths_to_intermediate(paths, Some(tile))
                .and_then(|()| self.draw_paths_from_intermediate(paths));
            self.active_path_tile = None;
            return result;
        }
        let mut bounds = first.clipped_bounds();
        for path in paths.iter().skip(1) {
            bounds = bounds.union(&path.clipped_bounds());
        }
        let group = RoundedContentGroup {
            bounds,
            clip_bounds: self
                .active_target_layout()
                .context("Path batch in empty group")?
                .bounds(),
            corner_radii: Corners::default(),
            corner_radii_y: Corners::default(),
            mask: None,
            completion: None,
            content_ready: true,
            retained: None,
        };
        let Some(layout) = rounded_target_layout(&group, self.active_target_layout())? else {
            return Ok(());
        };
        let old_path_bytes = self
            .rounded_content_targets
            .last()
            .and_then(|t| t.resources.as_ref())
            .and_then(|r| r.paths.as_ref())
            .map_or(0, |p| p.bytes);
        let available = ROUNDED_CONTENT_MAX_BYTES - (self.rounded_content_bytes - old_path_bytes);
        for tile in rounded_path_tile_layouts(layout, available)? {
            self.draw_paths_to_intermediate(paths, Some(tile))?;
            self.draw_paths_from_intermediate(paths)?;
        }
        Ok(())
    }

    fn straight_blend<'a, T>(&'a self, pipeline: &'a PipelineState<T>) -> &'a ID3D11BlendState {
        if self.rounded_content_targets.is_empty() {
            &pipeline.blend_state
        } else {
            &self.pipelines.group_straight_blend
        }
    }

    fn premultiplied_blend<'a, T>(
        &'a self,
        pipeline: &'a PipelineState<T>,
    ) -> &'a ID3D11BlendState {
        if self.rounded_content_targets.is_empty() {
            &pipeline.blend_state
        } else {
            &self.pipelines.group_premultiplied_blend
        }
    }

    #[inline]
    fn present(&mut self) -> Result<()> {
        let swap_chain = self
            .resources
            .swap_chain
            .as_ref()
            .context("DirectX swap chain unavailable")?;
        let result = unsafe { swap_chain.Present(0, DXGI_PRESENT(0)) };
        result.ok().context("Presenting swap chain failed")
    }

    pub(crate) fn physical_size(&self) -> (u32, u32) {
        (self.resources.width, self.resources.height)
    }

    fn present_or_defer(&mut self) -> Result<()> {
        if std::mem::take(&mut self.defer_next_present) {
            self.present_pending = true;
            return Ok(());
        }
        self.present()
    }

    /// 下一次绘制只渲染、不 Present，由 `present_deferred` 提交。
    pub(crate) fn defer_next_present(&mut self) {
        self.defer_next_present = true;
        self.present_pending = false;
    }

    pub(crate) fn present_pending(&self) -> bool {
        self.present_pending
    }

    /// 提交推迟的帧并清除推迟标记；没有待提交帧或 Present 失败时返回 false。
    pub(crate) fn present_deferred(&mut self) -> bool {
        self.defer_next_present = false;
        std::mem::take(&mut self.present_pending) && self.present().log_err().is_some()
    }

    /// 插入事件查询并提交命令，等 GPU 执行到这里（含之前的 Present）；超时返回 false。
    pub(crate) fn wait_for_gpu(&self, timeout: Duration) -> bool {
        let desc = D3D11_QUERY_DESC {
            Query: D3D11_QUERY_EVENT,
            MiscFlags: 0,
        };
        let mut query = None;
        if unsafe { self.devices.device.CreateQuery(&desc, Some(&mut query)) }.is_err() {
            return false;
        }
        let Some(query) = query else {
            return false;
        };
        let context = &self.devices.device_context;
        unsafe { context.End(&query) };
        let deadline = Instant::now() + timeout;
        loop {
            let mut done = windows::core::BOOL(0);
            // 首次不带 DONOTFLUSH，顺带把命令缓冲提交给 GPU。
            let polled = unsafe {
                context.GetData(
                    &query,
                    Some(&mut done as *mut windows::core::BOOL as *mut _),
                    std::mem::size_of::<windows::core::BOOL>() as u32,
                    0,
                )
            };
            if polled.is_err() {
                return false;
            }
            if done.as_bool() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
    }

    pub(crate) fn request_frame_capture(&mut self, region: [u32; 4]) -> Result<()> {
        anyhow::ensure!(
            self.frame_capture_request.is_none() && self.frame_capture_result.is_none(),
            "A frame capture is pending or has not been taken"
        );
        self.frame_capture_layout(region)?;
        anyhow::ensure!(
            self.resources.render_target.is_some(),
            "The frame capture render target is unavailable"
        );
        self.frame_capture_request = Some(region);
        Ok(())
    }

    pub(crate) fn take_frame_capture(&mut self) -> Option<Result<Vec<u8>>> {
        self.frame_capture_result.take()
    }

    fn frame_capture_layout(&self, [x, y, width, height]: [u32; 4]) -> Result<(usize, usize)> {
        anyhow::ensure!(width != 0 && height != 0, "Frame capture region is empty");
        let right = x.checked_add(width).context("Frame capture x overflow")?;
        let bottom = y.checked_add(height).context("Frame capture y overflow")?;
        anyhow::ensure!(
            right <= self.resources.width && bottom <= self.resources.height,
            "Frame capture region is outside the renderer viewport"
        );
        let row_bytes = (width as usize)
            .checked_mul(4)
            .context("Frame capture row length overflow")?;
        let bytes = row_bytes
            .checked_mul(height as usize)
            .context("Frame capture byte length overflow")?;
        anyhow::ensure!(bytes <= isize::MAX as usize, "Frame capture is too large");
        Ok((row_bytes, bytes))
    }

    fn invalidate_frame_capture(&mut self, reason: impl Into<String>) {
        if self.frame_capture_request.take().is_some() || self.frame_capture_result.is_some() {
            self.frame_capture_result = Some(Err(anyhow::anyhow!(reason.into())));
        }
    }

    fn capture_frame_region(&self, region: [u32; 4]) -> Result<Vec<u8>> {
        let (row_bytes, bytes) = self.frame_capture_layout(region)?;
        let [x, y, width, height] = region;
        let source = self
            .resources
            .render_target
            .as_ref()
            .context("Frame capture render target is unavailable")?;
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut source_desc) };
        anyhow::ensure!(
            source_desc.Format == RENDER_TARGET_FORMAT
                && source_desc.SampleDesc.Count == 1
                && x + width <= source_desc.Width
                && y + height <= source_desc.Height,
            "Frame capture backbuffer format or dimensions are invalid"
        );
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(bytes)
            .context("Allocating frame capture bytes")?;
        bgra.resize(bytes, 0);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: RENDER_TARGET_FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..Default::default()
        };
        let context = &self.devices.device_context;
        let mut staging = None;
        unsafe {
            self.devices
                .device
                .CreateTexture2D(&desc, None, Some(&mut staging))
        }
        .context("Creating frame capture staging texture")?;
        let staging = staging.context("Frame capture staging texture is missing")?;
        let source_box = D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + width,
            bottom: y + height,
            back: 1,
        };
        unsafe {
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, source, 0, Some(&source_box));
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // 同一 immediate context 内的同步 Map 等待上述绘制及拷贝完成，不读取桌面合成。
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .context("Mapping frame capture staging texture")?;
        let _mapped = MappedFrameCapture {
            context,
            texture: &staging,
        };
        let row_pitch = mapped.RowPitch as usize;
        anyhow::ensure!(
            !mapped.pData.is_null() && row_pitch >= row_bytes,
            "Frame capture mapped row pitch is invalid"
        );
        let mapped_bytes = row_pitch
            .checked_mul(height as usize - 1)
            .and_then(|length| length.checked_add(row_bytes))
            .context("Frame capture mapped byte length overflow")?;
        anyhow::ensure!(
            mapped_bytes <= isize::MAX as usize,
            "Frame capture mapped data is too large"
        );
        // 显卡行间 padding 不写入返回值；映射守卫覆盖所有校验/复制的退出路径。
        let source_bytes =
            unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), mapped_bytes) };
        for (row, target) in bgra.chunks_exact_mut(row_bytes).enumerate() {
            let offset = row * row_pitch;
            target.copy_from_slice(&source_bytes[offset..offset + row_bytes]);
        }
        Ok(bgra)
    }

    pub(crate) fn handle_device_lost(&mut self, directx_devices: &DirectXDevices) {
        try_to_recover_from_device_lost(
            || {
                self.handle_device_lost_impl(directx_devices, false)
                    .context("DirectXRenderer handling device lost")
            },
            |_| {},
            || {
                log::error!(
                    "DirectXRenderer failed to recover from device lost after multiple attempts"
                );
                // Do something here?
                // At this point, the device loss is considered unrecoverable.
            },
        );
    }

    fn handle_device_lost_impl(
        &mut self,
        directx_devices: &DirectXDevices,
        inject_failure: bool,
    ) -> Result<()> {
        publish_content_completions(&mut self.frame_content_completions, [0, 0], false);
        self.invalidate_frame_capture("Frame capture invalidated by device recovery");
        self.discard_rounded_content_targets();
        let (width, height) = (self.resources.width, self.resources.height);
        let disable_direct_composition = self.disable_direct_composition;

        unsafe {
            #[cfg(debug_assertions)]
            report_live_objects(&self.devices.device)
                .context("Failed to report live objects after device lost")
                .log_err();

            // 重试期间保持 resources/devices 槽合法；仅清空可选资源。
            self.devices.device_context.ClearState();
            self.devices.device_context.Flush();

            #[cfg(debug_assertions)]
            report_live_objects(&self.devices.device)
                .context("Failed to report live objects after device lost")
                .log_err();
        }
        // visual 的 SetContent 持有 swapchain；先释放 HWND composition 归属。
        drop(self.direct_composition.take());
        drop(self.resources.render_target_view[0].take());
        drop(self.resources.render_target.take());
        drop(self.resources.paths.take());
        self.active_path_tile = None;
        self.path_tiles.clear();
        self.sweep_retained_layers(true);
        drop(self.resources.swap_chain.take());

        let devices = DirectXRendererDevices::new(directx_devices, disable_direct_composition)
            .context("Recreating DirectX devices")?;
        let resources = DirectXResources::new(
            &devices,
            width,
            height,
            self.hwnd,
            disable_direct_composition,
            self.eager_paths,
            self.trace_startup,
        )?;
        if inject_failure {
            anyhow::bail!("QA injected resource recovery failure after resource creation");
        }
        let globals = DirectXGlobalElements::new(&devices.device)?;
        let pipelines = DirectXRenderPipelines::new(&devices.device)?;

        let direct_composition = if disable_direct_composition {
            None
        } else {
            let composition =
                DirectComposition::new(devices.dxgi_device.as_ref().unwrap(), self.hwnd)?;
            composition.set_swap_chain(
                resources
                    .swap_chain
                    .as_ref()
                    .context("DirectX swap chain unavailable")?,
            )?;
            Some(composition)
        };

        self.atlas
            .handle_device_lost(&devices.device, &devices.device_context);
        // 新 bundle 全部成功才替换；任一 ? 失败由普通局部 RAII 回收，旧空槽可继续重试。
        unsafe {
            ManuallyDrop::drop(&mut self.resources);
            ManuallyDrop::drop(&mut self.devices);
        }
        self.devices = ManuallyDrop::new(devices);
        self.resources = ManuallyDrop::new(resources);
        self.globals = globals;
        self.pipelines = pipelines;
        self.direct_composition = direct_composition;

        unsafe {
            self.devices
                .device_context
                .OMSetRenderTargets(Some(&self.resources.render_target_view), None);
        }
        Ok(())
    }

    fn probe_resource_recovery(&mut self) -> Result<()> {
        self.resource_recovery_probe = false;
        let devices = DirectXDevices {
            adapter: self.devices.adapter.clone(),
            dxgi_factory: self.devices.dxgi_factory.clone(),
            device: self.devices.device.clone(),
            device_context: self.devices.device_context.clone(),
        };
        // 仅在显式 QA 环境内模拟资源重建失败，不声称设备被拔出或重建设备。
        let failure = self.handle_device_lost_impl(&devices, true);
        let injected_failure = failure.as_ref().is_err_and(|error| {
            error.to_string() == "QA injected resource recovery failure after resource creation"
        });
        let empty_after_failure = self.resources.swap_chain.is_none()
            && self.resources.render_target.is_none()
            && self.resources.render_target_view[0].is_none()
            && self.resources.paths.is_none()
            && self.direct_composition.is_none();
        let retry = self.handle_device_lost_impl(&devices, false);
        let retry_ok = retry.is_ok();
        println!(
            "{{\"phase\":\"gpui_resource_recovery_probe\",\"injected_failure\":{injected_failure},\"empty_after_failure\":{empty_after_failure},\"retry_ok\":{retry_ok}}}"
        );
        retry.context("QA resource recovery retry failed")?;
        anyhow::ensure!(
            injected_failure && empty_after_failure,
            "QA resource recovery did not reach the injected failure safely"
        );
        Ok(())
    }

    pub(crate) fn draw(&mut self, scene: &Scene) -> Result<()> {
        let capture_requested = self.frame_capture_request.is_some();
        reset_scene_content_completions(scene);
        publish_content_completions(&mut self.frame_content_completions, [0, 0], false);
        let result = self.draw_frame(scene);
        // 任一路径失败都不 Present。先解除 MSAA/组 SRV，再回主目标及全窗口常量。
        self.discard_rounded_content_targets();
        let restored = self.bind_active_target();
        let result = match (result, restored) {
            (Err(error), Err(restore)) => {
                Err(error.context(format!("Restoring render target also failed: {restore:#}")))
            }
            (Err(error), _) => Err(error),
            (Ok(()), restored) => restored,
        }
        .and_then(|()| self.present_or_defer());
        publish_content_completions(
            &mut self.frame_content_completions,
            [self.resources.width, self.resources.height],
            result.is_ok(),
        );
        if result.is_ok() && self.first_draw {
            self.first_draw = false;
            if self.trace_startup {
                println!(
                    "{{\"phase\":\"gpui_first_draw\",\"paths\":{},\"path_resources\":{},\"width\":{},\"height\":{}}}",
                    scene.paths.len(),
                    self.resources.paths.is_some(),
                    self.resources.width,
                    self.resources.height,
                );
            }
        }
        if let (true, Err(error)) = (capture_requested, &result) {
            self.invalidate_frame_capture(format!("Frame capture rendering failed: {error:#}"));
        }
        result
    }

    fn draw_frame(&mut self, scene: &Scene) -> Result<()> {
        if self.resource_recovery_probe && !scene.paths.is_empty() {
            // 资源探针使用独立合成 paths/quads；设备恢复会清 atlas，旧 scene 的 tile 不再有效。
            anyhow::ensure!(
                scene.monochrome_sprites.is_empty()
                    && scene.polychrome_sprites.is_empty()
                    && scene.paths.iter().all(|path| path.image.is_none())
                    && scene.surfaces.is_empty(),
                "QA resource recovery probe requires a synthetic paths/quads scene without atlas sprites"
            );
            self.probe_resource_recovery()?;
        }
        anyhow::ensure!(
            self.rounded_content_targets.is_empty(),
            "Rounded content stack leaked between frames"
        );
        self.pre_draw()?;
        // >0 时位于复用的保留层内部：原语与嵌套组都已在纹理里，只等对应 End 合成。
        let mut skipping = 0_usize;
        for batch in scene.batches() {
            if skipping > 0 {
                match &batch {
                    PrimitiveBatch::StartRoundedContentGroup(_) => {
                        skipping += 1;
                        continue;
                    }
                    PrimitiveBatch::EndRoundedContentGroup => {
                        skipping -= 1;
                        if skipping > 0 {
                            continue;
                        }
                    }
                    _ => continue,
                }
            }
            // 空组仍消费并验证嵌套 Begin/End，不绘制到父窗口、不分配替身纹理。
            if self.active_target_layout().is_none()
                && !matches!(
                    &batch,
                    PrimitiveBatch::StartRoundedContentGroup(_)
                        | PrimitiveBatch::EndRoundedContentGroup
                )
            {
                continue;
            }
            match batch {
                PrimitiveBatch::Shadows(shadows) => self.draw_shadows(shadows),
                PrimitiveBatch::Quads(quads) => self.draw_quads(quads),
                PrimitiveBatch::Paths(paths) => self.draw_paths(paths),
                PrimitiveBatch::Underlines(underlines) => self.draw_underlines(underlines),
                PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    sprites,
                } => self.draw_monochrome_sprites(texture_id, sprites),
                PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites,
                } => self.draw_polychrome_sprites(texture_id, sprites),
                PrimitiveBatch::Surfaces(surfaces) => self.draw_surfaces(surfaces),
                PrimitiveBatch::StartRoundedContentGroup(group) => {
                    self.start_rounded_content_group(group).map(|reuse| {
                        if reuse {
                            skipping = 1;
                        }
                    })
                }
                PrimitiveBatch::EndRoundedContentGroup => self.end_rounded_content_group(),
            }.context(format!("scene too large: {} paths, {} shadows, {} quads, {} underlines, {} mono, {} poly, {} surfaces",
                    scene.paths.len(),
                    scene.shadows.len(),
                    scene.quads.len(),
                    scene.underlines.len(),
                    scene.monochrome_sprites.len(),
                    scene.polychrome_sprites.len(),
                    scene.surfaces.len(),))?;
        }
        anyhow::ensure!(
            self.rounded_content_targets.is_empty(),
            "Unbalanced rounded content Start"
        );
        self.sweep_retained_layers(false);
        if let Some(region) = self.frame_capture_request.take() {
            self.frame_capture_result = Some(self.capture_frame_region(region));
        }
        Ok(())
    }

    pub(crate) fn resize(&mut self, new_size: Size<DevicePixels>) -> Result<()> {
        let width = new_size.width.0.max(1) as u32;
        let height = new_size.height.0.max(1) as u32;
        if self.resources.width == width
            && self.resources.height == height
            && self.resources.render_target_view[0].is_some()
        {
            return Ok(());
        }
        publish_content_completions(&mut self.frame_content_completions, [0, 0], false);
        // 交换链重建会丢弃后台缓冲，推迟的帧不再有效。
        self.present_pending = false;
        self.invalidate_frame_capture("Frame capture invalidated by viewport resize");
        self.discard_rounded_content_targets();
        let started = self.trace_startup.then(Instant::now);

        // Clear the render target before resizing
        unsafe {
            self.devices.device_context.OMSetRenderTargets(None, None);
            // 上一帧 PathSprite 的 SRV 仍可能在 slot 0，解绑后才能释放旧尺寸纹理。
            self.devices
                .device_context
                .PSSetShaderResources(0, Some(&[None]));
            self.devices
                .device_context
                .VSSetShaderResources(0, Some(&[None]));
        }
        drop(self.resources.render_target_view[0].take());
        drop(self.resources.render_target.take());
        drop(self.resources.paths.take());
        self.active_path_tile = None;
        self.path_tiles.clear();
        self.sweep_retained_layers(true);

        // Resizing the swap chain requires a call to the underlying DXGI adapter, which can return the device removed error.
        // The app might have moved to a monitor that's attached to a different graphics device.
        // When a graphics device is removed or reset, the desktop resolution often changes, resulting in a window size change.
        // But here we just return the error, because we are handling device lost scenarios elsewhere.
        unsafe {
            self.resources
                .swap_chain
                .as_ref()
                .context("DirectX swap chain unavailable after recovery")?
                .ResizeBuffers(
                    BUFFER_COUNT as u32,
                    width,
                    height,
                    RENDER_TARGET_FORMAT,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
                .context("Failed to resize swap chain")?;
        }

        self.resources.recreate_resources(
            &self.devices,
            width,
            height,
            self.eager_paths,
            self.trace_startup,
        )?;
        unsafe {
            self.devices
                .device_context
                .OMSetRenderTargets(Some(&self.resources.render_target_view), None);
        }
        trace_renderer_stage("gpui_renderer_resize", started, width, height);

        Ok(())
    }

    fn draw_shadows(&mut self, shadows: &[Shadow]) -> Result<()> {
        if shadows.is_empty() {
            return Ok(());
        }
        self.pipelines.shadow_pipeline.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            shadows,
        )?;
        self.pipelines.shadow_pipeline.draw_with_blend(
            &self.devices.device_context,
            &self.active_viewport(),
            &self.globals.global_params_buffer,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            4,
            shadows.len() as u32,
            self.straight_blend(&self.pipelines.shadow_pipeline),
        )
    }

    fn draw_quads(&mut self, quads: &[Quad]) -> Result<()> {
        if quads.is_empty() {
            return Ok(());
        }
        self.pipelines.quad_pipeline.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            quads,
        )?;
        self.pipelines.quad_pipeline.draw_with_blend(
            &self.devices.device_context,
            &self.active_viewport(),
            &self.globals.global_params_buffer,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            4,
            quads.len() as u32,
            self.straight_blend(&self.pipelines.quad_pipeline),
        )
    }

    fn draw_paths_to_intermediate(
        &mut self,
        paths: &[Path<ScaledPixels>],
        layout: Option<RoundedTargetLayout>,
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let coverage_union = paths[0].coverage_union;
        let analytic_coverage = paths[0].analytic_coverage;
        anyhow::ensure!(
            !analytic_coverage || coverage_union,
            "Analytic coverage requires an individual union path"
        );
        anyhow::ensure!(
            !coverage_union || paths.len() == 1,
            "Coverage union requires an individual path batch"
        );
        anyhow::ensure!(
            !coverage_union || matches!(paths[0].color.tag, crate::color::BackgroundTag::Solid),
            "Coverage union requires a solid path color"
        );
        self.ensure_active_paths(layout)?;
        let path_layout = self.active_path_layout()?;
        self.set_target_globals(path_layout)?;
        let path_resources = self.active_paths()?;
        let coverage_view = if analytic_coverage {
            path_resources.intermediate_view.clone()
        } else {
            path_resources.intermediate_msaa_view.clone()
        };
        let intermediate_texture = path_resources.intermediate_texture.clone();
        let intermediate_msaa_texture = path_resources.intermediate_msaa_texture.clone();

        // 解除上一笔 resolved SRV，ResolveSubresource 不对仍绑定读槽的纹理写入。
        // MSAA 与组色目标分离；全局坐标/viewport 仍属于当前组。
        unsafe {
            self.devices
                .device_context
                .VSSetShaderResources(0, Some(&[None]));
            self.devices
                .device_context
                .PSSetShaderResources(0, Some(&[None]));
        }
        // 显式解析路径直接写单采样覆盖率；其它路径继续写 MSAA 再 resolve。
        unsafe {
            self.devices
                .device_context
                .ClearRenderTargetView(coverage_view[0].as_ref().unwrap(), &[0.0; 4]);
            self.devices
                .device_context
                .OMSetRenderTargets(Some(&coverage_view), None);
        }

        // Collect all vertices and sprites for a single draw call
        let mut vertices = Vec::new();

        for path in paths {
            vertices.extend(path.vertices.iter().map(|v| PathRasterizationSprite {
                xy_position: v.xy_position,
                st_position: v.st_position,
                // 单笔的边缘三角形先合并覆盖率，透明度留到最终合成时应用一次。
                color: if coverage_union {
                    Background::from(crate::rgba(0xffffffff))
                } else {
                    path.color
                },
                bounds: path.clipped_bounds(),
            }));
        }

        self.pipelines.path_rasterization_pipeline.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            &vertices,
        )?;
        self.pipelines.path_rasterization_pipeline.draw_with_blend(
            &self.devices.device_context,
            &path_layout.viewport(),
            &self.globals.global_params_buffer,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
            vertices.len() as u32,
            1,
            if coverage_union {
                &self.pipelines.path_union_blend_state
            } else {
                &self.pipelines.path_rasterization_pipeline.blend_state
            },
        )?;

        // Resolve MSAA to non-MSAA intermediate texture
        if !analytic_coverage {
            unsafe {
                self.devices.device_context.ResolveSubresource(
                    &intermediate_texture,
                    0,
                    &intermediate_msaa_texture,
                    0,
                    RENDER_TARGET_FORMAT,
                );
            }
        }
        // 组内恢复当前组，不把路径误画到窗口主目标。
        self.bind_active_target()?;
        Ok(())
    }

    fn draw_paths_from_intermediate(&mut self, paths: &[Path<ScaledPixels>]) -> Result<()> {
        let Some(first_path) = paths.first() else {
            return Ok(());
        };
        let intermediate_srv = self.active_paths()?.intermediate_srv.clone();
        let intermediate_bounds = self.active_path_layout()?.bounds();
        let tile_clip = self.active_path_clip_layout()?.bounds();
        let grouped = !self.rounded_content_targets.is_empty();
        let clip_to_tile = |bounds: Bounds<ScaledPixels>| {
            if grouped {
                bounds.intersect(&tile_clip)
            } else {
                bounds
            }
        };

        // When copying paths from the intermediate texture to the drawable,
        // each pixel must only be copied once, in case of transparent paths.
        //
        // If all paths have the same draw order, then their bounds are all
        // disjoint, so we can copy each path's bounds individually. If this
        // batch combines different draw orders, we perform a single copy
        // for a minimal spanning rect.
        let sprites = if paths.last().unwrap().order == first_path.order {
            paths
                .iter()
                .map(|path| PathSprite {
                    bounds: clip_to_tile(path.clipped_bounds()),
                    intermediate_bounds,
                    ..PathSprite::default()
                })
                .collect::<Vec<_>>()
        } else {
            let mut bounds = first_path.clipped_bounds();
            for path in paths.iter().skip(1) {
                bounds = bounds.union(&path.clipped_bounds());
            }
            vec![PathSprite {
                bounds: clip_to_tile(bounds),
                intermediate_bounds,
                ..PathSprite::default()
            }]
        };

        // 图像画刷单独成批，防止颜色画刷越过相邻路径的绘制顺序。
        let mut sprites = sprites;
        if first_path.coverage_union {
            sprites[0].coverage_union = 1;
            sprites[0].tint = first_path.color.solid.into();
        }
        let brush_view = if let Some(image) = &first_path.image {
            let sprite = &mut sprites[0];
            // 分块边界只裁最终无 MSAA 矩形，不裁路径覆盖率，避免缩放后接缝叠两次 alpha。
            sprite.bounds = sprite.bounds.intersect(&image.clip_bounds);
            sprite.image = 1;
            sprite.brush_bounds = image.bounds;
            sprite.brush_tile = Bounds {
                origin: image.tile.bounds.origin.map(|v| ScaledPixels(v.0 as f32)),
                size: image.tile.bounds.size.map(|v| ScaledPixels(v.0 as f32)),
            };
            self.atlas.get_texture_view(image.tile.texture_id)
        } else {
            [None]
        };

        self.pipelines.path_sprite_pipeline.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            &sprites,
        )?;

        // Draw the sprites with the path texture
        unsafe {
            self.devices
                .device_context
                .PSSetShaderResources(2, Some(&brush_view));
        }
        let result = self
            .pipelines
            .path_sprite_pipeline
            .draw_with_texture_and_blend(
                &self.devices.device_context,
                &intermediate_srv,
                &self.active_viewport(),
                &self.globals.global_params_buffer,
                &self.globals.sampler,
                sprites.len() as u32,
                self.premultiplied_blend(&self.pipelines.path_sprite_pipeline),
            );
        // 解除 atlas 引用，尺寸变更及设备恢复时不残留画刷资源。
        unsafe {
            self.devices
                .device_context
                .PSSetShaderResources(2, Some(&[None]));
        }
        result
    }

    fn draw_underlines(&mut self, underlines: &[Underline]) -> Result<()> {
        if underlines.is_empty() {
            return Ok(());
        }
        self.pipelines.underline_pipeline.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            underlines,
        )?;
        self.pipelines.underline_pipeline.draw_with_blend(
            &self.devices.device_context,
            &self.active_viewport(),
            &self.globals.global_params_buffer,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            4,
            underlines.len() as u32,
            self.straight_blend(&self.pipelines.underline_pipeline),
        )
    }

    fn draw_monochrome_sprites(
        &mut self,
        texture_id: AtlasTextureId,
        sprites: &[MonochromeSprite],
    ) -> Result<()> {
        if sprites.is_empty() {
            return Ok(());
        }
        self.pipelines.mono_sprites.update_buffer(
            &self.devices.device,
            &self.devices.device_context,
            sprites,
        )?;
        let texture_view = self.atlas.get_texture_view(texture_id);
        self.pipelines.mono_sprites.draw_with_texture_and_blend(
            &self.devices.device_context,
            &texture_view,
            &self.active_viewport(),
            &self.globals.global_params_buffer,
            &self.globals.sampler,
            sprites.len() as u32,
            self.straight_blend(&self.pipelines.mono_sprites),
        )
    }

    fn draw_polychrome_sprites(
        &mut self,
        texture_id: AtlasTextureId,
        sprites: &[PolychromeSprite],
    ) -> Result<()> {
        if sprites.is_empty() {
            return Ok(());
        }
        let viewport = self.active_viewport();
        let pipeline = if self.rounded_content_targets.is_empty() {
            &mut self.pipelines.poly_sprites
        } else {
            &mut self.pipelines.grouped_poly_sprites
        };
        pipeline.update_buffer(&self.devices.device, &self.devices.device_context, sprites)?;
        let texture_view = self.atlas.get_texture_view(texture_id);
        pipeline.draw_with_texture(
            &self.devices.device_context,
            &texture_view,
            &viewport,
            &self.globals.global_params_buffer,
            &self.globals.sampler,
            sprites.len() as u32,
        )
    }

    fn draw_surfaces(&mut self, surfaces: &[PaintSurface]) -> Result<()> {
        if surfaces.is_empty() {
            return Ok(());
        }
        Ok(())
    }

    pub(crate) fn gpu_specs(&self) -> Result<GpuSpecs> {
        let desc = unsafe { self.devices.adapter.GetDesc1() }?;
        let is_software_emulated = (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
        let device_name = String::from_utf16_lossy(&desc.Description)
            .trim_matches(char::from(0))
            .to_string();
        let driver_name = match desc.VendorId {
            0x10DE => "NVIDIA Corporation".to_string(),
            0x1002 => "AMD Corporation".to_string(),
            0x8086 => "Intel Corporation".to_string(),
            id => format!("Unknown Vendor (ID: {:#X})", id),
        };
        let driver_version = match desc.VendorId {
            0x10DE => nvidia::get_driver_version(),
            0x1002 => amd::get_driver_version(),
            // For Intel and other vendors, we use the DXGI API to get the driver version.
            _ => dxgi::get_driver_version(&self.devices.adapter),
        }
        .context("Failed to get gpu driver info")
        .log_err()
        .unwrap_or("Unknown Driver".to_string());
        Ok(GpuSpecs {
            is_software_emulated,
            device_name,
            driver_name,
            driver_info: driver_version,
        })
    }

    pub(crate) fn get_font_info() -> &'static FontInfo {
        static CACHED_FONT_INFO: OnceLock<FontInfo> = OnceLock::new();
        CACHED_FONT_INFO.get_or_init(|| unsafe {
            let factory: IDWriteFactory5 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).unwrap();
            let render_params: IDWriteRenderingParams1 =
                factory.CreateRenderingParams().unwrap().cast().unwrap();
            FontInfo {
                gamma_ratios: Self::get_gamma_ratios(render_params.GetGamma()),
                grayscale_enhanced_contrast: render_params.GetGrayscaleEnhancedContrast(),
            }
        })
    }

    // Gamma ratios for brightening/darkening edges for better contrast
    // https://github.com/microsoft/terminal/blob/1283c0f5b99a2961673249fa77c6b986efb5086c/src/renderer/atlas/dwrite.cpp#L50
    fn get_gamma_ratios(gamma: f32) -> [f32; 4] {
        const GAMMA_INCORRECT_TARGET_RATIOS: [[f32; 4]; 13] = [
            [0.0000 / 4.0, 0.0000 / 4.0, 0.0000 / 4.0, 0.0000 / 4.0], // gamma = 1.0
            [0.0166 / 4.0, -0.0807 / 4.0, 0.2227 / 4.0, -0.0751 / 4.0], // gamma = 1.1
            [0.0350 / 4.0, -0.1760 / 4.0, 0.4325 / 4.0, -0.1370 / 4.0], // gamma = 1.2
            [0.0543 / 4.0, -0.2821 / 4.0, 0.6302 / 4.0, -0.1876 / 4.0], // gamma = 1.3
            [0.0739 / 4.0, -0.3963 / 4.0, 0.8167 / 4.0, -0.2287 / 4.0], // gamma = 1.4
            [0.0933 / 4.0, -0.5161 / 4.0, 0.9926 / 4.0, -0.2616 / 4.0], // gamma = 1.5
            [0.1121 / 4.0, -0.6395 / 4.0, 1.1588 / 4.0, -0.2877 / 4.0], // gamma = 1.6
            [0.1300 / 4.0, -0.7649 / 4.0, 1.3159 / 4.0, -0.3080 / 4.0], // gamma = 1.7
            [0.1469 / 4.0, -0.8911 / 4.0, 1.4644 / 4.0, -0.3234 / 4.0], // gamma = 1.8
            [0.1627 / 4.0, -1.0170 / 4.0, 1.6051 / 4.0, -0.3347 / 4.0], // gamma = 1.9
            [0.1773 / 4.0, -1.1420 / 4.0, 1.7385 / 4.0, -0.3426 / 4.0], // gamma = 2.0
            [0.1908 / 4.0, -1.2652 / 4.0, 1.8650 / 4.0, -0.3476 / 4.0], // gamma = 2.1
            [0.2031 / 4.0, -1.3864 / 4.0, 1.9851 / 4.0, -0.3501 / 4.0], // gamma = 2.2
        ];

        const NORM13: f32 = ((0x10000 as f64) / (255.0 * 255.0) * 4.0) as f32;
        const NORM24: f32 = ((0x100 as f64) / (255.0) * 4.0) as f32;

        let index = ((gamma * 10.0).round() as usize).clamp(10, 22) - 10;
        let ratios = GAMMA_INCORRECT_TARGET_RATIOS[index];

        [
            ratios[0] * NORM13,
            ratios[1] * NORM24,
            ratios[2] * NORM13,
            ratios[3] * NORM24,
        ]
    }
}

impl DirectXResources {
    pub fn new(
        devices: &DirectXRendererDevices,
        width: u32,
        height: u32,
        hwnd: HWND,
        disable_direct_composition: bool,
        eager_paths: bool,
        trace_startup: bool,
    ) -> Result<Self> {
        let swap_chain = if disable_direct_composition {
            create_swap_chain(&devices.dxgi_factory, &devices.device, hwnd, width, height)?
        } else {
            create_swap_chain_for_composition(
                &devices.dxgi_factory,
                &devices.device,
                width,
                height,
            )?
        };

        let (render_target, render_target_view, paths, viewport) = create_resources(
            devices,
            &swap_chain,
            width,
            height,
            eager_paths,
            trace_startup,
        )?;
        set_rasterizer_state(&devices.device, &devices.device_context)?;

        Ok(Self {
            swap_chain: Some(swap_chain),
            render_target: Some(render_target),
            render_target_view,
            paths,
            viewport,
            width,
            height,
        })
    }

    #[inline]
    fn recreate_resources(
        &mut self,
        devices: &DirectXRendererDevices,
        width: u32,
        height: u32,
        eager_paths: bool,
        trace_startup: bool,
    ) -> Result<()> {
        // 全部成功才安装；失败时留下空 target，可由下一次同尺寸 resize 重试。
        let (render_target, render_target_view, paths, viewport) = create_resources(
            devices,
            self.swap_chain
                .as_ref()
                .context("DirectX swap chain unavailable after resize")?,
            width,
            height,
            eager_paths,
            trace_startup,
        )?;
        self.render_target = Some(render_target);
        self.render_target_view = render_target_view;
        self.paths = paths;
        self.viewport = viewport;
        self.width = width;
        self.height = height;
        Ok(())
    }
    fn ensure_paths(&mut self, device: &ID3D11Device, trace_startup: bool) -> Result<()> {
        if self.paths.is_none() {
            self.paths = Some(PathResources::new(
                device,
                self.width,
                self.height,
                trace_startup,
            )?);
        }
        Ok(())
    }
}

impl PathResources {
    fn new(device: &ID3D11Device, width: u32, height: u32, trace_startup: bool) -> Result<Self> {
        let started = trace_startup.then(Instant::now);
        let (intermediate_texture, intermediate_srv) =
            create_path_intermediate_texture(device, width, height)?;
        // 复用 resolve 纹理承载显式解析路径；只增加视图，不增加纹理或预算。
        let mut intermediate_view = None;
        unsafe {
            device.CreateRenderTargetView(
                &intermediate_texture,
                None,
                Some(&mut intermediate_view),
            )?;
        }
        let (intermediate_msaa_texture, intermediate_msaa_view) =
            create_path_intermediate_msaa_texture_and_view(device, width, height)?;
        if let Some(started) = started {
            let bytes =
                u64::from(width) * u64::from(height) * 4 * (1 + u64::from(PATH_MULTISAMPLE_COUNT));
            println!(
                "{{\"phase\":\"startup_stage\",\"stage\":\"gpui_path_resources_create\",\"width\":{width},\"height\":{height},\"bytes\":{bytes},\"duration_ms\":{:.3}}}",
                started.elapsed().as_secs_f64() * 1000.,
            );
        }
        Ok(Self {
            intermediate_texture,
            intermediate_srv,
            intermediate_view: [intermediate_view],
            intermediate_msaa_texture,
            intermediate_msaa_view,
        })
    }
}

fn trace_renderer_stage(stage: &str, started: Option<Instant>, width: u32, height: u32) {
    if let Some(started) = started {
        println!(
            "{{\"phase\":\"startup_stage\",\"stage\":\"{stage}\",\"width\":{width},\"height\":{height},\"duration_ms\":{:.3}}}",
            started.elapsed().as_secs_f64() * 1000.,
        );
    }
}

impl DirectXRenderPipelines {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let shadow_pipeline = PipelineState::new(
            device,
            "shadow_pipeline",
            ShaderModule::Shadow,
            4,
            create_blend_state(device)?,
        )?;
        let quad_pipeline = PipelineState::new(
            device,
            "quad_pipeline",
            ShaderModule::Quad,
            64,
            create_blend_state(device)?,
        )?;
        let path_rasterization_pipeline = PipelineState::new(
            device,
            "path_rasterization_pipeline",
            ShaderModule::PathRasterization,
            32,
            create_blend_state_for_path_rasterization(device)?,
        )?;
        let path_union_blend_state = create_blend_state_for_path_union(device)?;
        let path_sprite_pipeline = PipelineState::new(
            device,
            "path_sprite_pipeline",
            ShaderModule::PathSprite,
            4,
            create_blend_state_for_path_sprite(device)?,
        )?;
        let underline_pipeline = PipelineState::new(
            device,
            "underline_pipeline",
            ShaderModule::Underline,
            4,
            create_blend_state(device)?,
        )?;
        let mono_sprites = PipelineState::new(
            device,
            "monochrome_sprite_pipeline",
            ShaderModule::MonochromeSprite,
            512,
            create_blend_state(device)?,
        )?;
        let poly_sprites = PipelineState::new(
            device,
            "polychrome_sprite_pipeline",
            ShaderModule::PolychromeSprite,
            16,
            create_blend_state(device)?,
        )?;

        let grouped_poly_sprites = PipelineState::new(
            device,
            "grouped_polychrome_sprite_pipeline",
            ShaderModule::GroupedPolychromeSprite,
            16,
            create_blend_state_for_content_group(device, true)?,
        )?;
        let rounded_content = PipelineState::new(
            device,
            "rounded_content_group_pipeline",
            ShaderModule::RoundedContentGroup,
            1,
            create_blend_state_for_content_group(device, true)?,
        )?;
        let group_straight_blend = create_blend_state_for_content_group(device, false)?;
        let group_premultiplied_blend = create_blend_state_for_content_group(device, true)?;

        Ok(Self {
            shadow_pipeline,
            quad_pipeline,
            path_rasterization_pipeline,
            path_union_blend_state,
            path_sprite_pipeline,
            underline_pipeline,
            mono_sprites,
            poly_sprites,
            grouped_poly_sprites,
            rounded_content,
            group_straight_blend,
            group_premultiplied_blend,
        })
    }
}

impl DirectComposition {
    pub fn new(dxgi_device: &IDXGIDevice, hwnd: HWND) -> Result<Self> {
        let comp_device = get_comp_device(dxgi_device)?;
        let comp_target = unsafe { comp_device.CreateTargetForHwnd(hwnd, true) }?;
        let comp_visual = unsafe { comp_device.CreateVisual() }?;

        Ok(Self {
            comp_device,
            comp_target,
            comp_visual,
        })
    }

    pub fn set_swap_chain(&self, swap_chain: &IDXGISwapChain1) -> Result<()> {
        unsafe {
            self.comp_visual.SetContent(swap_chain)?;
            self.comp_target.SetRoot(&self.comp_visual)?;
            self.comp_device.Commit()?;
        }
        Ok(())
    }
}

impl DirectXGlobalElements {
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        let global_params_buffer = unsafe {
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<GlobalParams>() as u32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut buffer = None;
            device.CreateBuffer(&desc, None, Some(&mut buffer))?;
            [buffer]
        };

        let sampler = unsafe {
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_WRAP,
                AddressV: D3D11_TEXTURE_ADDRESS_WRAP,
                AddressW: D3D11_TEXTURE_ADDRESS_WRAP,
                MipLODBias: 0.0,
                MaxAnisotropy: 1,
                ComparisonFunc: D3D11_COMPARISON_ALWAYS,
                BorderColor: [0.0; 4],
                MinLOD: 0.0,
                MaxLOD: D3D11_FLOAT32_MAX,
            };
            let mut output = None;
            device.CreateSamplerState(&desc, Some(&mut output))?;
            [output]
        };

        Ok(Self {
            global_params_buffer,
            sampler,
        })
    }
}

#[derive(Debug, Default)]
#[repr(C)]
struct GlobalParams {
    gamma_ratios: [f32; 4],
    viewport_size: [f32; 2],
    grayscale_enhanced_contrast: f32,
    rounded_content: u32,
    render_target_origin: [f32; 2],
    _pad: [u32; 2],
}

struct PipelineState<T> {
    label: &'static str,
    vertex: ID3D11VertexShader,
    fragment: ID3D11PixelShader,
    buffer: ID3D11Buffer,
    buffer_size: usize,
    view: [Option<ID3D11ShaderResourceView>; 1],
    blend_state: ID3D11BlendState,
    _marker: std::marker::PhantomData<T>,
}

impl<T> PipelineState<T> {
    fn new(
        device: &ID3D11Device,
        label: &'static str,
        shader_module: ShaderModule,
        buffer_size: usize,
        blend_state: ID3D11BlendState,
    ) -> Result<Self> {
        let vertex = {
            let raw_shader = RawShaderBytes::new(shader_module, ShaderTarget::Vertex)?;
            create_vertex_shader(device, raw_shader.as_bytes())?
        };
        let fragment = {
            let raw_shader = RawShaderBytes::new(shader_module, ShaderTarget::Fragment)?;
            create_fragment_shader(device, raw_shader.as_bytes())?
        };
        let buffer = create_buffer(device, std::mem::size_of::<T>(), buffer_size)?;
        let view = create_buffer_view(device, &buffer)?;

        Ok(PipelineState {
            label,
            vertex,
            fragment,
            buffer,
            buffer_size,
            view,
            blend_state,
            _marker: std::marker::PhantomData,
        })
    }

    fn update_buffer(
        &mut self,
        device: &ID3D11Device,
        device_context: &ID3D11DeviceContext,
        data: &[T],
    ) -> Result<()> {
        if self.buffer_size < data.len() {
            let new_buffer_size = data.len().next_power_of_two();
            log::info!(
                "Updating {} buffer size from {} to {}",
                self.label,
                self.buffer_size,
                new_buffer_size
            );
            let buffer = create_buffer(device, std::mem::size_of::<T>(), new_buffer_size)?;
            let view = create_buffer_view(device, &buffer)?;
            self.buffer = buffer;
            self.view = view;
            self.buffer_size = new_buffer_size;
        }
        update_buffer(device_context, &self.buffer, data)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_with_blend(
        &self,
        device_context: &ID3D11DeviceContext,
        viewport: &[D3D11_VIEWPORT],
        global_params: &[Option<ID3D11Buffer>],
        topology: D3D_PRIMITIVE_TOPOLOGY,
        vertex_count: u32,
        instance_count: u32,
        blend_state: &ID3D11BlendState,
    ) -> Result<()> {
        set_pipeline_state(
            device_context,
            &self.view,
            topology,
            viewport,
            &self.vertex,
            &self.fragment,
            global_params,
            blend_state,
        );
        unsafe {
            device_context.DrawInstanced(vertex_count, instance_count, 0, 0);
        }
        Ok(())
    }

    fn draw_with_texture(
        &self,
        device_context: &ID3D11DeviceContext,
        texture: &[Option<ID3D11ShaderResourceView>],
        viewport: &[D3D11_VIEWPORT],
        global_params: &[Option<ID3D11Buffer>],
        sampler: &[Option<ID3D11SamplerState>],
        instance_count: u32,
    ) -> Result<()> {
        self.draw_with_texture_and_blend(
            device_context,
            texture,
            viewport,
            global_params,
            sampler,
            instance_count,
            &self.blend_state,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_with_texture_and_blend(
        &self,
        device_context: &ID3D11DeviceContext,
        texture: &[Option<ID3D11ShaderResourceView>],
        viewport: &[D3D11_VIEWPORT],
        global_params: &[Option<ID3D11Buffer>],
        sampler: &[Option<ID3D11SamplerState>],
        instance_count: u32,
        blend_state: &ID3D11BlendState,
    ) -> Result<()> {
        set_pipeline_state(
            device_context,
            &self.view,
            D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
            viewport,
            &self.vertex,
            &self.fragment,
            global_params,
            blend_state,
        );
        unsafe {
            device_context.PSSetSamplers(0, Some(sampler));
            device_context.VSSetShaderResources(0, Some(texture));
            device_context.PSSetShaderResources(0, Some(texture));

            device_context.DrawInstanced(4, instance_count, 0, 0);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
#[repr(C)]
struct PathRasterizationSprite {
    xy_position: Point<ScaledPixels>,
    st_position: Point<f32>,
    color: Background,
    bounds: Bounds<ScaledPixels>,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct RoundedContentSprite {
    bounds: Bounds<ScaledPixels>,
    contour_bounds: Bounds<ScaledPixels>,
    clip_bounds: Bounds<ScaledPixels>,
    corner_radii: Corners<ScaledPixels>,
    corner_radii_y: Corners<ScaledPixels>,
    has_mask: u32,
    padding: [u32; 3],
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct PathSprite {
    bounds: Bounds<ScaledPixels>,
    intermediate_bounds: Bounds<ScaledPixels>,
    brush_bounds: Bounds<ScaledPixels>,
    brush_tile: Bounds<ScaledPixels>,
    tint: Rgba,
    image: u32,
    coverage_union: u32,
    padding: [u32; 2],
}

impl Drop for DirectXRenderer {
    fn drop(&mut self) {
        publish_content_completions(&mut self.frame_content_completions, [0, 0], false);
        self.discard_rounded_content_targets();
        #[cfg(debug_assertions)]
        report_live_objects(&self.devices.device).ok();
        unsafe {
            ManuallyDrop::drop(&mut self.resources);
            ManuallyDrop::drop(&mut self.devices);
        }
        if std::env::var_os("RUSTSHOT_GPUI_LIFECYCLE_TRACE").is_some() {
            use std::io::Write as _;
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis());
            let _ = writeln!(
                std::io::stdout().lock(),
                "{{\"phase\":\"gpui_renderer_drop\",\"hwnd\":{},\"unix_ms\":{unix_ms}}}",
                self.hwnd.0 as usize,
            );
        }
    }
}

#[inline]
fn get_comp_device(dxgi_device: &IDXGIDevice) -> Result<IDCompositionDevice> {
    Ok(unsafe { DCompositionCreateDevice(dxgi_device)? })
}

fn create_swap_chain_for_composition(
    dxgi_factory: &IDXGIFactory6,
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<IDXGISwapChain1> {
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: RENDER_TARGET_FORMAT,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: BUFFER_COUNT as u32,
        // Composition SwapChains only support the DXGI_SCALING_STRETCH Scaling.
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    };
    Ok(unsafe { dxgi_factory.CreateSwapChainForComposition(device, &desc, None)? })
}

fn create_swap_chain(
    dxgi_factory: &IDXGIFactory6,
    device: &ID3D11Device,
    hwnd: HWND,
    width: u32,
    height: u32,
) -> Result<IDXGISwapChain1> {
    use windows::Win32::Graphics::Dxgi::DXGI_MWA_NO_ALT_ENTER;

    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: width,
        Height: height,
        Format: RENDER_TARGET_FORMAT,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: BUFFER_COUNT as u32,
        Scaling: DXGI_SCALING_NONE,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    let swap_chain =
        unsafe { dxgi_factory.CreateSwapChainForHwnd(device, hwnd, &desc, None, None) }?;
    unsafe { dxgi_factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }?;
    Ok(swap_chain)
}

#[inline]
fn create_resources(
    devices: &DirectXRendererDevices,
    swap_chain: &IDXGISwapChain1,
    width: u32,
    height: u32,
    eager_paths: bool,
    trace_startup: bool,
) -> Result<(
    ID3D11Texture2D,
    [Option<ID3D11RenderTargetView>; 1],
    Option<PathResources>,
    [D3D11_VIEWPORT; 1],
)> {
    let (render_target, render_target_view) =
        create_render_target_and_its_view(swap_chain, &devices.device)?;
    let paths = if eager_paths {
        Some(PathResources::new(
            &devices.device,
            width,
            height,
            trace_startup,
        )?)
    } else {
        None
    };
    let viewport = set_viewport(&devices.device_context, width as f32, height as f32);
    Ok((render_target, render_target_view, paths, viewport))
}

#[inline]
fn create_render_target_and_its_view(
    swap_chain: &IDXGISwapChain1,
    device: &ID3D11Device,
) -> Result<(ID3D11Texture2D, [Option<ID3D11RenderTargetView>; 1])> {
    let render_target: ID3D11Texture2D = unsafe { swap_chain.GetBuffer(0) }?;
    let mut render_target_view = None;
    unsafe { device.CreateRenderTargetView(&render_target, None, Some(&mut render_target_view))? };
    Ok((render_target, [Some(render_target_view.unwrap())]))
}

#[inline]
fn create_path_intermediate_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, [Option<ID3D11ShaderResourceView>; 1])> {
    let texture = unsafe {
        let mut output = None;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: RENDER_TARGET_FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        device.CreateTexture2D(&desc, None, Some(&mut output))?;
        output.unwrap()
    };

    let mut shader_resource_view = None;
    unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut shader_resource_view))? };

    Ok((texture, [Some(shader_resource_view.unwrap())]))
}

#[inline]
fn create_path_intermediate_msaa_texture_and_view(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, [Option<ID3D11RenderTargetView>; 1])> {
    let msaa_texture = unsafe {
        let mut output = None;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: RENDER_TARGET_FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: PATH_MULTISAMPLE_COUNT,
                Quality: D3D11_STANDARD_MULTISAMPLE_PATTERN.0 as u32,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        device.CreateTexture2D(&desc, None, Some(&mut output))?;
        output.unwrap()
    };
    let mut msaa_view = None;
    unsafe { device.CreateRenderTargetView(&msaa_texture, None, Some(&mut msaa_view))? };
    Ok((msaa_texture, [Some(msaa_view.unwrap())]))
}

#[inline]
fn set_viewport(
    device_context: &ID3D11DeviceContext,
    width: f32,
    height: f32,
) -> [D3D11_VIEWPORT; 1] {
    let viewport = [D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: width,
        Height: height,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }];
    unsafe { device_context.RSSetViewports(Some(&viewport)) };
    viewport
}

#[inline]
fn set_rasterizer_state(device: &ID3D11Device, device_context: &ID3D11DeviceContext) -> Result<()> {
    let desc = D3D11_RASTERIZER_DESC {
        FillMode: D3D11_FILL_SOLID,
        CullMode: D3D11_CULL_NONE,
        FrontCounterClockwise: false.into(),
        DepthBias: 0,
        DepthBiasClamp: 0.0,
        SlopeScaledDepthBias: 0.0,
        DepthClipEnable: true.into(),
        ScissorEnable: false.into(),
        MultisampleEnable: true.into(),
        AntialiasedLineEnable: false.into(),
    };
    let rasterizer_state = unsafe {
        let mut state = None;
        device.CreateRasterizerState(&desc, Some(&mut state))?;
        state.unwrap()
    };
    unsafe { device_context.RSSetState(&rasterizer_state) };
    Ok(())
}

// https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ns-d3d11-d3d11_blend_desc
#[inline]
fn create_blend_state(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    // If the feature level is set to greater than D3D_FEATURE_LEVEL_9_3, the display
    // device performs the blend in linear space, which is ideal.
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_SRC_ALPHA;
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        Ok(state.unwrap())
    }
}

#[inline]
fn create_blend_state_for_content_group(
    device: &ID3D11Device,
    premultiplied: bool,
) -> Result<ID3D11BlendState> {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = if premultiplied {
        D3D11_BLEND_ONE
    } else {
        D3D11_BLEND_SRC_ALPHA
    };
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    let mut state = None;
    unsafe {
        device.CreateBlendState(&desc, Some(&mut state))?;
    }
    state.context("Creating rounded content blend state")
}

#[inline]
fn create_blend_state_for_path_rasterization(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    // If the feature level is set to greater than D3D_FEATURE_LEVEL_9_3, the display
    // device performs the blend in linear space, which is ideal.
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_ONE;
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        Ok(state.unwrap())
    }
}

#[inline]
fn create_blend_state_for_path_union(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    // 白色遮罩逐 MSAA 样本取最大覆盖率，避免同一笔自交或 AA 外沿重复累加。
    // 普通 GPUI 路径仍使用原先的 source-over，不合并不同对象的透明度。
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_MAX;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_MAX;
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_ONE;
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        Ok(state.unwrap())
    }
}

#[inline]
fn create_blend_state_for_path_sprite(device: &ID3D11Device) -> Result<ID3D11BlendState> {
    // If the feature level is set to greater than D3D_FEATURE_LEVEL_9_3, the display
    // device performs the blend in linear space, which is ideal.
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0].BlendEnable = true.into();
    desc.RenderTarget[0].BlendOp = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].BlendOpAlpha = D3D11_BLEND_OP_ADD;
    desc.RenderTarget[0].SrcBlend = D3D11_BLEND_ONE;
    desc.RenderTarget[0].SrcBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].DestBlend = D3D11_BLEND_INV_SRC_ALPHA;
    desc.RenderTarget[0].DestBlendAlpha = D3D11_BLEND_ONE;
    desc.RenderTarget[0].RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8;
    unsafe {
        let mut state = None;
        device.CreateBlendState(&desc, Some(&mut state))?;
        Ok(state.unwrap())
    }
}

#[inline]
fn create_vertex_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11VertexShader> {
    unsafe {
        let mut shader = None;
        device.CreateVertexShader(bytes, None, Some(&mut shader))?;
        Ok(shader.unwrap())
    }
}

#[inline]
fn create_fragment_shader(device: &ID3D11Device, bytes: &[u8]) -> Result<ID3D11PixelShader> {
    unsafe {
        let mut shader = None;
        device.CreatePixelShader(bytes, None, Some(&mut shader))?;
        Ok(shader.unwrap())
    }
}

#[inline]
fn create_buffer(
    device: &ID3D11Device,
    element_size: usize,
    buffer_size: usize,
) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: (element_size * buffer_size) as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: element_size as u32,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }?;
    Ok(buffer.unwrap())
}

#[inline]
fn create_buffer_view(
    device: &ID3D11Device,
    buffer: &ID3D11Buffer,
) -> Result<[Option<ID3D11ShaderResourceView>; 1]> {
    let mut view = None;
    unsafe { device.CreateShaderResourceView(buffer, None, Some(&mut view)) }?;
    Ok([view])
}

#[inline]
fn update_buffer<T>(
    device_context: &ID3D11DeviceContext,
    buffer: &ID3D11Buffer,
    data: &[T],
) -> Result<()> {
    unsafe {
        let mut dest = std::mem::zeroed();
        device_context.Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut dest))?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), dest.pData as _, data.len());
        device_context.Unmap(buffer, 0);
    }
    Ok(())
}

#[inline]
fn set_pipeline_state(
    device_context: &ID3D11DeviceContext,
    buffer_view: &[Option<ID3D11ShaderResourceView>],
    topology: D3D_PRIMITIVE_TOPOLOGY,
    viewport: &[D3D11_VIEWPORT],
    vertex_shader: &ID3D11VertexShader,
    fragment_shader: &ID3D11PixelShader,
    global_params: &[Option<ID3D11Buffer>],
    blend_state: &ID3D11BlendState,
) {
    unsafe {
        device_context.VSSetShaderResources(1, Some(buffer_view));
        device_context.PSSetShaderResources(1, Some(buffer_view));
        device_context.IASetPrimitiveTopology(topology);
        device_context.RSSetViewports(Some(viewport));
        device_context.VSSetShader(vertex_shader, None);
        device_context.PSSetShader(fragment_shader, None);
        device_context.VSSetConstantBuffers(0, Some(global_params));
        device_context.PSSetConstantBuffers(0, Some(global_params));
        device_context.OMSetBlendState(blend_state, None, 0xFFFFFFFF);
    }
}

#[cfg(debug_assertions)]
fn report_live_objects(device: &ID3D11Device) -> Result<()> {
    let debug_device: ID3D11Debug = device.cast()?;
    unsafe {
        debug_device.ReportLiveDeviceObjects(D3D11_RLDO_DETAIL)?;
    }
    Ok(())
}

#[cfg(test)]
mod rounded_content_tests {
    use super::*;

    fn group(bounds: [f32; 4], clip: [f32; 4]) -> RoundedContentGroup {
        let rect = |v: [f32; 4]| Bounds {
            origin: point(ScaledPixels(v[0]), ScaledPixels(v[1])),
            size: size(ScaledPixels(v[2]), ScaledPixels(v[3])),
        };
        RoundedContentGroup {
            bounds: rect(bounds),
            clip_bounds: rect(clip),
            corner_radii: Corners::default(),
            corner_radii_y: Corners::default(),
            mask: None,
            completion: None,
            content_ready: true,
            retained: None,
        }
    }

    #[::core::prelude::v1::test]
    fn rounded_content_visible_roi_preserves_signed_fractional_geometry_and_nested_phase() {
        let parent = RoundedTargetLayout {
            origin: [0, 0],
            size: [1025, 769],
        };
        let outer = group(
            [-3.75, 7.25, 1300.5, 400.25],
            [12.125, 8.875, 920.625, 401.5],
        );
        let layout = rounded_target_layout(&outer, Some(parent))
            .unwrap()
            .unwrap();
        assert_eq!(
            layout,
            RoundedTargetLayout {
                origin: [12, 8],
                size: [921, 400]
            }
        );
        assert_eq!(outer.bounds.origin.x.0, -3.75);
        assert_eq!(outer.clip_bounds.origin.y.0, 8.875);
        let inner = group([30.5, 6.125, 300.25, 1000.0], [0.0, 0.0, 1025.0, 769.0]);
        let child = rounded_target_layout(&inner, Some(layout))
            .unwrap()
            .unwrap();
        assert_eq!(
            child,
            RoundedTargetLayout {
                origin: [30, 8],
                size: [301, 400]
            }
        );
        let bounds = child.bounds();
        assert_eq!(bounds.origin.x.0, 30.0);
        assert_eq!(bounds.size.width.0, 301.0);
        let viewport = child.viewport()[0];
        assert_eq!((viewport.Width, viewport.Height), (301.0, 400.0));
        // 组原点是整数，恢复绝对物理像素中心不改变半像素相位。
        let window_center = [77.5, 200.5];
        let local_center = [
            window_center[0] - child.origin[0] as f32,
            window_center[1] - child.origin[1] as f32,
        ];
        assert_eq!(
            [
                local_center[0] + child.origin[0] as f32,
                local_center[1] + child.origin[1] as f32
            ],
            window_center
        );
    }

    #[::core::prelude::v1::test]
    fn rounded_content_empty_nesting_still_rejects_bad_geometry_and_texture_limits() {
        let parent = RoundedTargetLayout {
            origin: [0, 0],
            size: [1025, 769],
        };
        let outside = group([-20.0, -30.0, 1.0, 1.0], [0.0, 0.0, 1025.0, 769.0]);
        assert!(
            rounded_target_layout(&outside, Some(parent))
                .unwrap()
                .is_none()
        );
        let valid = group([1.0, 2.0, 8.0, 9.0], [0.0, 0.0, 1025.0, 769.0]);
        assert!(rounded_target_layout(&valid, None).unwrap().is_none());
        let mut invalid = valid;
        invalid.corner_radii_y.bottom_left = ScaledPixels(f32::NAN);
        assert!(rounded_target_layout(&invalid, None).is_err());
        let negative = group([1.0, 2.0, -1.0, 9.0], [0.0, 0.0, 1025.0, 769.0]);
        assert!(rounded_target_layout(&negative, Some(parent)).is_err());
        let overflow = group([f32::MAX, 0.0, f32::MAX, 9.0], [0.0, 0.0, 1025.0, 769.0]);
        assert!(rounded_target_layout(&overflow, Some(parent)).is_err());
        let huge_parent = RoundedTargetLayout {
            origin: [0, 0],
            size: [17_000, 1],
        };
        let huge = group([0.0, 0.0, 17_000.0, 1.0], [0.0, 0.0, 17_000.0, 1.0]);
        assert!(rounded_target_layout(&huge, Some(huge_parent)).is_err());
    }

    #[::core::prelude::v1::test]
    fn rounded_content_budget_counts_real_bytes_and_tiles_large_legal_content_without_overlap() {
        let double_4k = 2 * 3840 * 2160;
        let color = rounded_resource_charge(0, double_4k, 1).unwrap();
        let paths =
            rounded_resource_charge(color, double_4k, 1 + u64::from(PATH_MULTISAMPLE_COUNT))
                .unwrap();
        assert_eq!(color + paths, 398_131_200);
        let canvas = RoundedTargetLayout {
            origin: [3, 7],
            size: [16_384, 2_048],
        };
        let color = rounded_resource_charge(0, canvas.pixels(), 1).unwrap();
        let tiles = rounded_path_tile_layouts(canvas, ROUNDED_CONTENT_MAX_BYTES - color).unwrap();
        assert_eq!(tiles.len(), 2);
        assert_eq!(
            tiles.iter().map(|t| t.pixels()).sum::<u64>(),
            canvas.pixels()
        );
        assert_eq!(tiles[0].origin, canvas.origin);
        assert_eq!(tiles[0].size[0], canvas.size[0]);
        assert_eq!(
            tiles[1].origin,
            [canvas.origin[0], canvas.origin[1] + tiles[0].size[1]]
        );
        assert_eq!(tiles[0].size[1] + tiles[1].size[1], canvas.size[1]);
        for tile in &tiles {
            assert!(
                rounded_resource_charge(
                    color,
                    tile.pixels(),
                    1 + u64::from(PATH_MULTISAMPLE_COUNT)
                )
                .is_ok()
            );
        }
        assert!(rounded_path_tile_layouts(canvas, 19).is_err());
        assert!(rounded_path_tile_layouts(canvas, 20).is_err());
        assert_eq!(
            rounded_resource_charge(0, ROUNDED_CONTENT_MAX_BYTES / 4, 1).unwrap(),
            ROUNDED_CONTENT_MAX_BYTES
        );
        assert!(rounded_resource_charge(ROUNDED_CONTENT_MAX_BYTES, 1, 1).is_err());
        assert!(rounded_resource_charge(0, u64::MAX, 2).is_err());
        assert!(rounded_resource_charge(u64::MAX, 1, 1).is_err());
    }

    fn mask_path() -> Path<ScaledPixels> {
        let mut path = Path::new(point(px(-3.75), px(5.25)));
        path.line_to(point(px(20.5), px(5.25)));
        path.line_to(point(px(20.5), px(30.75)));
        let mut path = path.scale(1.25);
        path.content_mask = ContentMask {
            bounds: group([0., 0., 100., 80.], [0., 0., 100., 80.]).bounds,
        };
        path
    }

    #[::core::prelude::v1::test]
    fn actual_content_group_mask_validation_keeps_signed_geometry_and_rejects_bad_vertices() {
        let valid = mask_path();
        validate_content_mask_path(&valid).unwrap();
        assert_eq!(valid.vertices[0].xy_position.x.0, -3.75 * 1.25);
        let mut bad = valid.clone();
        bad.vertices[0].xy_position.x = ScaledPixels(f32::NAN);
        assert!(validate_content_mask_path(&bad).is_err());
        let mut bad = valid.clone();
        bad.vertices[0].st_position.y = f32::INFINITY;
        assert!(validate_content_mask_path(&bad).is_err());
        let mut bad = valid.clone();
        bad.vertices.pop();
        assert!(validate_content_mask_path(&bad).is_err());
        let mut bad = valid.clone();
        bad.vertices.clear();
        assert!(validate_content_mask_path(&bad).is_err());
        let mut bad = valid;
        bad.content_mask.bounds.size.width = ScaledPixels(-1.);
        assert!(validate_content_mask_path(&bad).is_err());
    }

    #[::core::prelude::v1::test]
    fn actual_content_group_mask_budget_accounts_full_resolve_and_bounded_msaa_tiles() {
        let layout = RoundedTargetLayout {
            origin: [3, 7],
            size: [16_384, 3072],
        };
        let color = rounded_resource_charge(0, layout.pixels(), 1).unwrap();
        let (mask, tiles) = rounded_mask_tile_layouts(layout, color).unwrap();
        assert_eq!(mask, color);
        assert_eq!(color + mask, 384 * 1024 * 1024);
        assert_eq!(tiles.len(), 8);
        assert_eq!(
            tiles.iter().map(|t| t.pixels()).sum::<u64>(),
            layout.pixels()
        );
        let mut next_y = layout.origin[1];
        for tile in &tiles {
            assert_eq!(tile.origin, [layout.origin[0], next_y]);
            assert_eq!(tile.size[0], layout.size[0]);
            rounded_resource_charge(
                color + mask,
                tile.pixels(),
                1 + u64::from(PATH_MULTISAMPLE_COUNT),
            )
            .unwrap();
            next_y += tile.size[1];
        }
        assert_eq!(next_y, layout.origin[1] + layout.size[1]);
        // 工作MSAA/resolve退役后仅持有颜色及完整mask，后继Scene path可使用剩余额度。
        assert!(
            rounded_path_tile_layouts(layout, ROUNDED_CONTENT_MAX_BYTES - color - mask).is_ok()
        );
        assert!(rounded_mask_tile_layouts(layout, ROUNDED_CONTENT_MAX_BYTES - mask + 1).is_err());
    }

    #[::core::prelude::v1::test]
    fn actual_content_group_cpu_ready_gate_does_not_publish_unpresented_or_empty_content() {
        let layout = RoundedTargetLayout {
            origin: [0, 0],
            size: [101, 79],
        };
        let completion = Arc::new(AtomicU64::new(0));
        let mut tracked = group([1., 2., 8., 9.], [0., 0., 101., 79.]);
        tracked.completion = Some(completion.clone());
        tracked.content_ready = false;
        assert!(validate_content_group_ready(&tracked, Some(layout)).is_err());
        tracked.content_ready = true;
        assert!(validate_content_group_ready(&tracked, None).is_err());
        validate_content_group_ready(&tracked, Some(layout)).unwrap();
        assert_eq!(completion.load(Ordering::Acquire), 0);
        tracked.completion = None;
        validate_content_group_ready(&tracked, None).unwrap();
    }

    #[::core::prelude::v1::test]
    fn actual_content_group_scene_entry_reset_invalidates_old_success_before_any_start() {
        let first = Arc::new(AtomicU64::new(0));
        let nested = Arc::new(AtomicU64::new(41));
        let unrelated = Arc::new(AtomicU64::new(77));
        let mut previous_pending = vec![first.clone()];
        publish_content_completions(&mut previous_pending, [1931, 1079], true);
        assert!(previous_pending.is_empty());
        assert_ne!(first.load(Ordering::Acquire), 0);
        let mut outer = group([0., 0., 101., 79.], [0., 0., 101., 79.]);
        outer.completion = Some(first.clone());
        let mut child = outer.clone();
        child.completion = Some(nested.clone());
        child.content_ready = false;
        let mut scene = Scene::default();
        scene.push_rounded_content_group(outer);
        scene.push_rounded_content_group(child);
        scene.pop_rounded_content_group();
        scene.pop_rounded_content_group();
        scene.finish();
        let mut replayed = Scene::default();
        replayed.replay(0..scene.len(), &scene);
        replayed.finish();
        reset_scene_content_completions(&replayed);
        // 模拟入口之后pre_draw失败：不执行Start/成功publish，旧资格必须已归零。
        assert_eq!(first.load(Ordering::Acquire), 0);
        assert_eq!(nested.load(Ordering::Acquire), 0);
        assert_eq!(unrelated.load(Ordering::Acquire), 77);
        assert!(previous_pending.is_empty());
    }

    #[::core::prelude::v1::test]
    fn actual_content_group_completion_publication_requires_present_success_and_exact_size() {
        let first = Arc::new(AtomicU64::new(1));
        let nested = Arc::new(AtomicU64::new(2));
        let unrelated = Arc::new(AtomicU64::new(0));
        let mut pending = vec![first.clone(), nested.clone()];
        publish_content_completions(&mut pending, [1931, 1079], false);
        assert!(pending.is_empty());
        assert_eq!(first.load(Ordering::Acquire), 0);
        assert_eq!(nested.load(Ordering::Acquire), 0);
        let mut pending = vec![first.clone(), nested.clone()];
        publish_content_completions(&mut pending, [1931, 1079], true);
        let expected = (1931_u64 << 32) | 1079;
        assert_eq!(first.load(Ordering::Acquire), expected);
        assert_eq!(nested.load(Ordering::Acquire), expected);
        assert_eq!(unrelated.load(Ordering::Acquire), 0);
        assert!(pending.is_empty());
        let mut pending = vec![first.clone()];
        publish_content_completions(&mut pending, [0, 1079], true);
        assert_eq!(first.load(Ordering::Acquire), 0);
    }

    #[::core::prelude::v1::test]
    fn main_path_tiles_bucket_small_batches_and_fall_back_near_window_size() {
        let main = [3840, 2160];
        assert_eq!(path_tile_size([1, 1], main), Some([256, 256]));
        assert_eq!(path_tile_size([300, 40], main), Some([512, 256]));
        assert_eq!(path_tile_size([1500, 900], main), Some([2048, 1024]));
        // 接近整窗时分档面不比整窗中间面省，继续复用整窗资源。
        assert_eq!(path_tile_size([2100, 1200], main), None);
        assert_eq!(path_tile_size([200, 200], [300, 300]), None);
    }

    #[::core::prelude::v1::test]
    fn rounded_content_shader_wire_layout_keeps_constant_and_structured_buffer_alignment() {
        assert_eq!(std::mem::size_of::<GlobalParams>(), 48);
        assert_eq!(std::mem::offset_of!(GlobalParams, render_target_origin), 32);
        assert_eq!(std::mem::size_of::<RoundedContentSprite>(), 96);
        assert_eq!(std::mem::offset_of!(RoundedContentSprite, has_mask), 80);
        assert_eq!(std::mem::size_of::<PathSprite>(), 96);
        assert_eq!(std::mem::offset_of!(PathSprite, intermediate_bounds), 16);
        assert_eq!(std::mem::offset_of!(RoundedContentSprite, corner_radii), 48);
        assert_eq!(
            std::mem::offset_of!(RoundedContentSprite, corner_radii_y),
            64
        );
    }
}

const BUFFER_COUNT: usize = 3;

pub(crate) mod shader_resources {
    use anyhow::Result;

    #[cfg(all(debug_assertions, any(not(gpui_precompiled_shaders), test)))]
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    #[cfg(all(debug_assertions, not(gpui_precompiled_shaders)))]
    use std::sync::OnceLock;

    #[cfg(all(debug_assertions, any(not(gpui_precompiled_shaders), test)))]
    use windows::{
        Win32::Graphics::Direct3D::{
            Fxc::{D3DCOMPILE_DEBUG, D3DCOMPILE_SKIP_OPTIMIZATION, D3DCompile},
            ID3DBlob,
        },
        core::PCSTR,
    };

    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    pub(crate) enum ShaderModule {
        Quad,
        Shadow,
        Underline,
        PathRasterization,
        PathSprite,
        MonochromeSprite,
        PolychromeSprite,
        GroupedPolychromeSprite,
        RoundedContentGroup,
        EmojiRasterization,
    }

    #[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
    pub(crate) enum ShaderTarget {
        Vertex,
        Fragment,
    }

    #[cfg(all(debug_assertions, any(not(gpui_precompiled_shaders), test)))]
    #[derive(Default)]
    struct ShaderBytecodeCache {
        entries: Mutex<HashMap<(ShaderModule, ShaderTarget), Arc<[u8]>>>,
    }

    #[cfg(all(debug_assertions, any(not(gpui_precompiled_shaders), test)))]
    impl ShaderBytecodeCache {
        fn get_or_compile(
            &self,
            module: ShaderModule,
            target: ShaderTarget,
            compile: impl FnOnce() -> Result<Vec<u8>>,
        ) -> Result<Arc<[u8]>> {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(bytes) = entries.get(&(module, target)) {
                return Ok(bytes.clone());
            }

            // 只缓存成功的不可变字节码；设备资源和可变管线缓冲仍属于每个窗口。
            // 首次编译持锁，避免两个初始化调用为同一入口重复编译。
            let bytes: Arc<[u8]> = compile()?.into();
            entries.insert((module, target), bytes.clone());
            Ok(bytes)
        }
    }

    pub(crate) struct RawShaderBytes {
        #[cfg(any(not(debug_assertions), gpui_precompiled_shaders))]
        inner: &'static [u8],

        #[cfg(all(debug_assertions, not(gpui_precompiled_shaders)))]
        inner: Arc<[u8]>,
    }

    impl RawShaderBytes {
        pub(crate) fn new(module: ShaderModule, target: ShaderTarget) -> Result<Self> {
            #[cfg(any(not(debug_assertions), gpui_precompiled_shaders))]
            {
                Ok(Self::from_bytes(module, target))
            }
            #[cfg(all(debug_assertions, not(gpui_precompiled_shaders)))]
            {
                static BYTECODE_CACHE: OnceLock<ShaderBytecodeCache> = OnceLock::new();
                let inner = BYTECODE_CACHE
                    .get_or_init(ShaderBytecodeCache::default)
                    .get_or_compile(module, target, || {
                        let blob = build_shader_blob(module, target)?;
                        let bytes = unsafe {
                            std::slice::from_raw_parts(
                                blob.GetBufferPointer() as *const u8,
                                blob.GetBufferSize(),
                            )
                        };
                        Ok(bytes.to_vec())
                    })?;
                Ok(Self { inner })
            }
        }

        pub(crate) fn as_bytes(&self) -> &[u8] {
            self.inner.as_ref()
        }

        #[cfg(any(not(debug_assertions), gpui_precompiled_shaders))]
        fn from_bytes(module: ShaderModule, target: ShaderTarget) -> Self {
            let bytes = match module {
                ShaderModule::Quad => match target {
                    ShaderTarget::Vertex => QUAD_VERTEX_BYTES,
                    ShaderTarget::Fragment => QUAD_FRAGMENT_BYTES,
                },
                ShaderModule::Shadow => match target {
                    ShaderTarget::Vertex => SHADOW_VERTEX_BYTES,
                    ShaderTarget::Fragment => SHADOW_FRAGMENT_BYTES,
                },
                ShaderModule::Underline => match target {
                    ShaderTarget::Vertex => UNDERLINE_VERTEX_BYTES,
                    ShaderTarget::Fragment => UNDERLINE_FRAGMENT_BYTES,
                },
                ShaderModule::PathRasterization => match target {
                    ShaderTarget::Vertex => PATH_RASTERIZATION_VERTEX_BYTES,
                    ShaderTarget::Fragment => PATH_RASTERIZATION_FRAGMENT_BYTES,
                },
                ShaderModule::PathSprite => match target {
                    ShaderTarget::Vertex => PATH_SPRITE_VERTEX_BYTES,
                    ShaderTarget::Fragment => PATH_SPRITE_FRAGMENT_BYTES,
                },
                ShaderModule::MonochromeSprite => match target {
                    ShaderTarget::Vertex => MONOCHROME_SPRITE_VERTEX_BYTES,
                    ShaderTarget::Fragment => MONOCHROME_SPRITE_FRAGMENT_BYTES,
                },
                ShaderModule::PolychromeSprite => match target {
                    ShaderTarget::Vertex => POLYCHROME_SPRITE_VERTEX_BYTES,
                    ShaderTarget::Fragment => POLYCHROME_SPRITE_FRAGMENT_BYTES,
                },
                ShaderModule::GroupedPolychromeSprite => match target {
                    ShaderTarget::Vertex => GROUPED_POLYCHROME_SPRITE_VERTEX_BYTES,
                    ShaderTarget::Fragment => GROUPED_POLYCHROME_SPRITE_FRAGMENT_BYTES,
                },
                ShaderModule::RoundedContentGroup => match target {
                    ShaderTarget::Vertex => ROUNDED_CONTENT_GROUP_VERTEX_BYTES,
                    ShaderTarget::Fragment => ROUNDED_CONTENT_GROUP_FRAGMENT_BYTES,
                },
                ShaderModule::EmojiRasterization => match target {
                    ShaderTarget::Vertex => EMOJI_RASTERIZATION_VERTEX_BYTES,
                    ShaderTarget::Fragment => EMOJI_RASTERIZATION_FRAGMENT_BYTES,
                },
            };
            Self { inner: bytes }
        }
    }

    #[cfg(all(debug_assertions, any(not(gpui_precompiled_shaders), test)))]
    pub(super) fn build_shader_blob(entry: ShaderModule, target: ShaderTarget) -> Result<ID3DBlob> {
        unsafe {
            // Debug 也必须使用与 Rust 缓冲结构同次编译的 HLSL。
            // 从工作区读文件会让已复制/运行的旧 EXE 消费后继着色器，破坏布局合同。
            let (shader_name, shader_source) = if matches!(entry, ShaderModule::EmojiRasterization) {
                (
                    b"color_text_raster.hlsl\0".as_slice(),
                    include_str!("color_text_raster.hlsl"),
                )
            } else {
                (b"shaders.hlsl\0".as_slice(), include_str!("shaders.hlsl"))
            };
            let shader_source = shader_source.replace(
                "#include \"alpha_correction.hlsl\"",
                include_str!("alpha_correction.hlsl"),
            );

            let entry = format!(
                "{}_{}\0",
                entry.as_str(),
                match target {
                    ShaderTarget::Vertex => "vertex",
                    ShaderTarget::Fragment => "fragment",
                }
            );
            let target = match target {
                ShaderTarget::Vertex => "vs_4_1\0",
                ShaderTarget::Fragment => "ps_4_1\0",
            };

            let mut compile_blob = None;
            let mut error_blob = None;
            let entry_point = PCSTR::from_raw(entry.as_ptr());
            let target_cstr = PCSTR::from_raw(target.as_ptr());

            let ret = D3DCompile(
                shader_source.as_ptr().cast(),
                shader_source.len(),
                PCSTR::from_raw(shader_name.as_ptr()),
                None,
                None,
                entry_point,
                target_cstr,
                D3DCOMPILE_DEBUG | D3DCOMPILE_SKIP_OPTIMIZATION,
                0,
                &mut compile_blob,
                Some(&mut error_blob),
            );
            if ret.is_err() {
                let Some(error_blob) = error_blob else {
                    return Err(anyhow::anyhow!("{ret:?}"));
                };

                let error_string =
                    std::ffi::CStr::from_ptr(error_blob.GetBufferPointer() as *const i8)
                        .to_string_lossy();
                log::error!("Shader compile error: {}", error_string);
                return Err(anyhow::anyhow!("Compile error: {}", error_string));
            }
            Ok(compile_blob.unwrap())
        }
    }

    #[cfg(any(not(debug_assertions), gpui_precompiled_shaders))]
    include!(concat!(env!("OUT_DIR"), "/shaders_bytes.rs"));

    #[cfg(all(test, debug_assertions))]
    mod embedded_shader_tests {
        use super::*;

        #[::core::prelude::v1::test]
        fn debug_shader_bytecode_cache_shares_success_by_entry() {
            let cache = ShaderBytecodeCache::default();
            let first = cache
                .get_or_compile(ShaderModule::Quad, ShaderTarget::Vertex, || Ok(vec![1]))
                .unwrap();
            let second = cache
                .get_or_compile(ShaderModule::Quad, ShaderTarget::Vertex, || {
                    panic!("已成功编译的入口不应重复编译")
                })
                .unwrap();
            assert!(Arc::ptr_eq(&first, &second));

            let other_target = cache
                .get_or_compile(ShaderModule::Quad, ShaderTarget::Fragment, || Ok(vec![2]))
                .unwrap();
            let other_module = cache
                .get_or_compile(ShaderModule::Shadow, ShaderTarget::Vertex, || Ok(vec![3]))
                .unwrap();
            assert_eq!(other_target.as_ref(), &[2]);
            assert_eq!(other_module.as_ref(), &[3]);
        }

        #[::core::prelude::v1::test]
        fn debug_shader_bytecode_cache_retries_failed_entry() {
            let cache = ShaderBytecodeCache::default();
            let failure = cache.get_or_compile(ShaderModule::Quad, ShaderTarget::Vertex, || {
                Err(anyhow::anyhow!("模拟编译失败"))
            });
            assert!(failure.is_err());

            let retry = cache
                .get_or_compile(ShaderModule::Quad, ShaderTarget::Vertex, || Ok(vec![4]))
                .unwrap();
            assert_eq!(retry.as_ref(), &[4]);
        }

        #[::core::prelude::v1::test]
        fn embedded_debug_shaders_compile_every_renderer_entry() {
            for module in [
                ShaderModule::Quad,
                ShaderModule::Shadow,
                ShaderModule::Underline,
                ShaderModule::PathRasterization,
                ShaderModule::PathSprite,
                ShaderModule::MonochromeSprite,
                ShaderModule::PolychromeSprite,
                ShaderModule::GroupedPolychromeSprite,
                ShaderModule::RoundedContentGroup,
                ShaderModule::EmojiRasterization,
            ] {
                for target in [ShaderTarget::Vertex, ShaderTarget::Fragment] {
                    let blob = build_shader_blob(module, target).unwrap_or_else(|error| {
                        panic!("{module:?}/{target:?} 的内置着色器编译失败：{error}")
                    });
                    assert!(unsafe { blob.GetBufferSize() } > 0);
                }
            }
        }
    }

    #[cfg(all(debug_assertions, any(not(gpui_precompiled_shaders), test)))]
    impl ShaderModule {
        pub fn as_str(&self) -> &str {
            match self {
                ShaderModule::Quad => "quad",
                ShaderModule::Shadow => "shadow",
                ShaderModule::Underline => "underline",
                ShaderModule::PathRasterization => "path_rasterization",
                ShaderModule::PathSprite => "path_sprite",
                ShaderModule::MonochromeSprite => "monochrome_sprite",
                ShaderModule::PolychromeSprite => "polychrome_sprite",
                ShaderModule::GroupedPolychromeSprite => "grouped_polychrome_sprite",
                ShaderModule::RoundedContentGroup => "rounded_content_group",
                ShaderModule::EmojiRasterization => "emoji_rasterization",
            }
        }
    }
}

mod nvidia {
    use std::{
        ffi::CStr,
        os::raw::{c_char, c_int, c_uint},
    };

    use anyhow::Result;
    use windows::{Win32::System::LibraryLoader::GetProcAddress, core::s};

    use crate::with_dll_library;

    // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_lite_common.h#L180
    const NVAPI_SHORT_STRING_MAX: usize = 64;

    // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_lite_common.h#L235
    #[allow(non_camel_case_types)]
    type NvAPI_ShortString = [c_char; NVAPI_SHORT_STRING_MAX];

    // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_lite_common.h#L447
    #[allow(non_camel_case_types)]
    type NvAPI_SYS_GetDriverAndBranchVersion_t = unsafe extern "C" fn(
        driver_version: *mut c_uint,
        build_branch_string: *mut NvAPI_ShortString,
    ) -> c_int;

    pub(super) fn get_driver_version() -> Result<String> {
        #[cfg(target_pointer_width = "64")]
        let nvidia_dll_name = s!("nvapi64.dll");
        #[cfg(target_pointer_width = "32")]
        let nvidia_dll_name = s!("nvapi.dll");

        with_dll_library(nvidia_dll_name, |nvidia_dll| unsafe {
            let nvapi_query_addr = GetProcAddress(nvidia_dll, s!("nvapi_QueryInterface"))
                .ok_or_else(|| anyhow::anyhow!("Failed to get nvapi_QueryInterface address"))?;
            let nvapi_query: extern "C" fn(u32) -> *mut () = std::mem::transmute(nvapi_query_addr);

            // https://github.com/NVIDIA/nvapi/blob/7cb76fce2f52de818b3da497af646af1ec16ce27/nvapi_interface.h#L41
            let nvapi_get_driver_version_ptr = nvapi_query(0x2926aaad);
            if nvapi_get_driver_version_ptr.is_null() {
                anyhow::bail!("Failed to get NVIDIA driver version function pointer");
            }
            let nvapi_get_driver_version: NvAPI_SYS_GetDriverAndBranchVersion_t =
                std::mem::transmute(nvapi_get_driver_version_ptr);

            let mut driver_version: c_uint = 0;
            let mut build_branch_string: NvAPI_ShortString = [0; NVAPI_SHORT_STRING_MAX];
            let result = nvapi_get_driver_version(
                &mut driver_version as *mut c_uint,
                &mut build_branch_string as *mut NvAPI_ShortString,
            );

            if result != 0 {
                anyhow::bail!(
                    "Failed to get NVIDIA driver version, error code: {}",
                    result
                );
            }
            let major = driver_version / 100;
            let minor = driver_version % 100;
            let branch_string = CStr::from_ptr(build_branch_string.as_ptr());
            Ok(format!(
                "{}.{} {}",
                major,
                minor,
                branch_string.to_string_lossy()
            ))
        })
    }
}

mod amd {
    use std::os::raw::{c_char, c_int, c_void};

    use anyhow::Result;
    use windows::{Win32::System::LibraryLoader::GetProcAddress, core::s};

    use crate::with_dll_library;

    // https://github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/blob/5d8812d703d0335741b6f7ffc37838eeb8b967f7/ags_lib/inc/amd_ags.h#L145
    const AGS_CURRENT_VERSION: i32 = (6 << 22) | (3 << 12);

    // https://github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/blob/5d8812d703d0335741b6f7ffc37838eeb8b967f7/ags_lib/inc/amd_ags.h#L204
    // This is an opaque type, using struct to represent it properly for FFI
    #[repr(C)]
    struct AGSContext {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct AGSGPUInfo {
        pub driver_version: *const c_char,
        pub radeon_software_version: *const c_char,
        pub num_devices: c_int,
        pub devices: *mut c_void,
    }

    // https://github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/blob/5d8812d703d0335741b6f7ffc37838eeb8b967f7/ags_lib/inc/amd_ags.h#L429
    #[allow(non_camel_case_types)]
    type agsInitialize_t = unsafe extern "C" fn(
        version: c_int,
        config: *const c_void,
        context: *mut *mut AGSContext,
        gpu_info: *mut AGSGPUInfo,
    ) -> c_int;

    // https://github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/blob/5d8812d703d0335741b6f7ffc37838eeb8b967f7/ags_lib/inc/amd_ags.h#L436
    #[allow(non_camel_case_types)]
    type agsDeInitialize_t = unsafe extern "C" fn(context: *mut AGSContext) -> c_int;

    pub(super) fn get_driver_version() -> Result<String> {
        #[cfg(target_pointer_width = "64")]
        let amd_dll_name = s!("amd_ags_x64.dll");
        #[cfg(target_pointer_width = "32")]
        let amd_dll_name = s!("amd_ags_x86.dll");

        with_dll_library(amd_dll_name, |amd_dll| unsafe {
            let ags_initialize_addr = GetProcAddress(amd_dll, s!("agsInitialize"))
                .ok_or_else(|| anyhow::anyhow!("Failed to get agsInitialize address"))?;
            let ags_deinitialize_addr = GetProcAddress(amd_dll, s!("agsDeInitialize"))
                .ok_or_else(|| anyhow::anyhow!("Failed to get agsDeInitialize address"))?;

            let ags_initialize: agsInitialize_t = std::mem::transmute(ags_initialize_addr);
            let ags_deinitialize: agsDeInitialize_t = std::mem::transmute(ags_deinitialize_addr);

            let mut context: *mut AGSContext = std::ptr::null_mut();
            let mut gpu_info: AGSGPUInfo = AGSGPUInfo {
                driver_version: std::ptr::null(),
                radeon_software_version: std::ptr::null(),
                num_devices: 0,
                devices: std::ptr::null_mut(),
            };

            let result = ags_initialize(
                AGS_CURRENT_VERSION,
                std::ptr::null(),
                &mut context,
                &mut gpu_info,
            );
            if result != 0 {
                anyhow::bail!("Failed to initialize AMD AGS, error code: {}", result);
            }

            // Vulkan actually returns this as the driver version
            let software_version = if !gpu_info.radeon_software_version.is_null() {
                std::ffi::CStr::from_ptr(gpu_info.radeon_software_version)
                    .to_string_lossy()
                    .into_owned()
            } else {
                "Unknown Radeon Software Version".to_string()
            };

            let driver_version = if !gpu_info.driver_version.is_null() {
                std::ffi::CStr::from_ptr(gpu_info.driver_version)
                    .to_string_lossy()
                    .into_owned()
            } else {
                "Unknown Radeon Driver Version".to_string()
            };

            ags_deinitialize(context);
            Ok(format!("{} ({})", software_version, driver_version))
        })
    }
}

mod dxgi {
    use windows::{
        Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice},
        core::Interface,
    };

    pub(super) fn get_driver_version(adapter: &IDXGIAdapter1) -> anyhow::Result<String> {
        let number = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID as _) }?;
        Ok(format!(
            "{}.{}.{}.{}",
            number >> 48,
            (number >> 32) & 0xFFFF,
            (number >> 16) & 0xFFFF,
            number & 0xFFFF
        ))
    }
}
