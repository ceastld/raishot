//! 冻结底图适配：DXGI 桌面保留 GPU 裁剪，静态桌面和文件复用原始 BGRA 帧。
use crate::{
    annotation::Bounds,
    capture::Image,
    desktop::DesktopCapture,
    fast_capture::FrozenDesktop,
    geometry::{Rect, ScreenRect},
    scene::{PixelPlane, SourcePixels},
    Result,
};
use gpui::RenderImage;
use std::sync::Arc;

enum Backend {
    Desktop(DesktopCapture),
    FrozenDesktop(FrozenDesktop),
    Image(Arc<RenderImage>),
    /// 贴图透明预览原图；带外观的长图显示由有界目标块负责。
    PresentationImage(Arc<RenderImage>),
    ImageTiles(Vec<ImageTile>),
    ProjectedImage {
        raw: Arc<RenderImage>,
        region: Rect,
        grid: ProjectionGrid,
        preserve_alpha: bool,
    },
    /// 矢量纯色底：不持有像素或纹理，绘制和导出按需取色。
    Solid([u8; 4]),
    /// 全截图 CPU 采样域与有界可见预览分离，不保存截图设备或纹理。
    CaptureContext {
        pixels: Arc<crate::pin_raster_document::capture_source::CapturePixels>,
        content: Rect,
    },
}
struct ImageTile {
    rect: Rect,
    original: Arc<RenderImage>,
    preview: Arc<RenderImage>,
}
const IMAGE_TILE_SIDE: u32 = 4096;
const MAX_IMAGE_SIDE: u32 = 65_535;
const PROJECTED_INITIAL_SIDE: u32 = 1024;
const PROJECTED_MAX_SIDE: u32 = 16384;
const PROJECTED_MAX_PIXELS: u64 = 8_000_000;
#[derive(Clone, Copy, Debug, PartialEq)]
struct ProjectionGrid {
    density: f64,
    pixel_bounds: Rect,
}
pub struct CanvasSource {
    pub bounds: ScreenRect,
    pub current: usize,
    pub capture_ms: f64,
    backend: Backend,
}
impl CanvasSource {
    pub(crate) fn capture_context(
        pixels: Arc<crate::pin_raster_document::capture_source::CapturePixels>,
        content: Rect,
    ) -> Result<(Self, Vec<Arc<RenderImage>>)> {
        pixels.validate()?;
        if !content.valid(pixels.bounds.width, pixels.bounds.height)
            || u64::from(content.width) * u64::from(content.height) > 32_000_000
        {
            return Err("截图贴图可见范围超出来源或输出预算".into());
        }
        // styled Pin 的主体缓存按实际可见目标采样，超长来源不上传整帧。
        let preview = if content.width <= 16_384
            && content.height <= 16_384
            && u64::from(content.width) * u64::from(content.height) <= PROJECTED_MAX_PIXELS
        {
            let image = pixels.read_content(content)?;
            render_image(image.width, image.height, image.bgra)
        } else {
            render_image(1, 1, vec![0; 4])
        };
        Ok((
            Self {
                bounds: pixels.bounds,
                current: 0,
                capture_ms: 0.,
                backend: Backend::CaptureContext { pixels, content },
            },
            vec![preview],
        ))
    }
    pub(crate) fn capture_pixels(
        &self,
    ) -> Option<&Arc<crate::pin_raster_document::capture_source::CapturePixels>> {
        match &self.backend {
            Backend::CaptureContext { pixels, .. } => Some(pixels),
            _ => None,
        }
    }
    /// 返回原有 CPU 帧的强引用；不保留 Desktop/FrozenDesktop 设备和会话。
    pub(crate) fn shared_cpu_planes(
        &self,
        previews: &[Arc<RenderImage>],
    ) -> Result<Arc<crate::pin_raster_document::capture_source::CapturePixels>> {
        use crate::pin_raster_document::capture_source::{CapturePixels, OwnedPlane};
        if let Some(pixels) = self.capture_pixels() {
            return Ok(Arc::clone(pixels));
        }
        let expected = if self.solid_fill().is_some() {
            0
        } else {
            self.screen_count()
        };
        if previews.len() != expected {
            return Err("截图 CPU 平面与原始屏幕数量不匹配".into());
        }
        let planes = previews
            .iter()
            .enumerate()
            .map(|(index, preview)| {
                let image = match &self.backend {
                    Backend::Image(raw)
                    | Backend::PresentationImage(raw)
                    | Backend::ProjectedImage { raw, .. } => raw.clone(),
                    Backend::ImageTiles(tiles) => tiles[index].original.clone(),
                    Backend::Desktop(_) | Backend::FrozenDesktop(_) | Backend::Solid(_) => {
                        preview.clone()
                    }
                    Backend::CaptureContext { .. } => unreachable!(),
                };
                OwnedPlane {
                    rect: self.original_rect(index),
                    image,
                }
            })
            .collect();
        CapturePixels::new(self.bounds, self.solid_fill(), planes).map(Arc::new)
    }
    pub fn desktop(desktop: DesktopCapture) -> Self {
        Self {
            bounds: desktop.bounds,
            current: desktop.current,
            capture_ms: desktop.capture_ms,
            backend: Backend::Desktop(desktop),
        }
    }
    pub fn frozen_desktop(desktop: FrozenDesktop) -> (Self, Vec<Arc<RenderImage>>) {
        let previews = desktop.screens.iter().map(|s| s.preview.clone()).collect();
        (
            Self {
                bounds: desktop.bounds,
                current: desktop.current,
                capture_ms: desktop.capture_ms,
                backend: Backend::FrozenDesktop(desktop),
            },
            previews,
        )
    }
    pub fn image(image: Image) -> Result<(Self, Vec<Arc<RenderImage>>)> {
        if image.width == 0
            || image.height == 0
            || image.width > MAX_IMAGE_SIDE
            || image.height > MAX_IMAGE_SIDE
            || image.width as u64 * image.height as u64 > 32_000_000
            || image.bgra.len() != image.width as usize * image.height as usize * 4
        {
            return Err(tr!(
                "图片尺寸无效或超过 3200 万像素 / 65535 单边限制",
                "Invalid image size, or over 32 megapixels / 65535 px per side"
            )
            .into());
        }
        if image.width > 16384 || image.height > 16384 {
            return Self::tiled_image(image.width, image.height, &image.bgra);
        }
        let bounds = ScreenRect {
            left: 0,
            top: 0,
            width: image.width,
            height: image.height,
        };
        let matte = image
            .bgra
            .chunks_exact(4)
            .any(|p| p[3] != 255)
            .then(|| checker_preview(&image, (0, 0)));
        let raw = render_image(image.width, image.height, image.bgra);
        let preview = matte.map_or_else(
            || raw.clone(),
            |bytes| render_image(image.width, image.height, bytes),
        );
        Ok((
            Self {
                bounds,
                current: 0,
                capture_ms: 0.,
                backend: Backend::Image(raw),
            },
            vec![preview],
        ))
    }
    /// 文字贴图等矢量文档的纯色底；没有显示预览，画布直接绘制色块。
    pub(crate) fn solid(width: u32, height: u32, bgra: [u8; 4]) -> Result<Self> {
        if width == 0
            || height == 0
            || width > MAX_IMAGE_SIDE
            || height > MAX_IMAGE_SIDE
            || u64::from(width) * u64::from(height) > 32_000_000
        {
            return Err(tr!(
                "纯色画布尺寸无效或超过 3200 万像素 / 65535 单边限制",
                "Invalid solid canvas size, or over 32 megapixels / 65535 px per side"
            )
            .into());
        }
        Ok(Self {
            bounds: ScreenRect {
                left: 0,
                top: 0,
                width,
                height,
            },
            current: 0,
            capture_ms: 0.,
            backend: Backend::Solid(bgra),
        })
    }
    pub(crate) fn solid_fill(&self) -> Option<[u8; 4]> {
        match self.backend {
            Backend::Solid(bgra) => Some(bgra),
            _ => None,
        }
    }
    /// 只换纯色底；返回是否改变，调用方负责使 Scene 失效。
    pub(crate) fn set_solid_fill(&mut self, bgra: [u8; 4]) -> bool {
        match &mut self.backend {
            Backend::Solid(current) if *current != bgra => {
                *current = bgra;
                true
            }
            _ => false,
        }
    }
    /// 图片编辑器长图移动原始 Vec，只投影显示区域；旧 image/tiled 工厂保持原合同。
    pub(crate) fn editor_image(image: Image) -> Result<(Self, Vec<Arc<RenderImage>>)> {
        let expected = (image.width as usize)
            .checked_mul(image.height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or("图片像素缓冲长度溢出")?;
        if image.width == 0
            || image.height == 0
            || image.width > MAX_IMAGE_SIDE
            || image.height > MAX_IMAGE_SIDE
            || u64::from(image.width) * u64::from(image.height) > 32_000_000
            || image.bgra.len() != expected
        {
            return Err(tr!(
                "图片尺寸无效或超过 3200 万像素 / 65535 单边限制",
                "Invalid image size, or over 32 megapixels / 65535 px per side"
            )
            .into());
        }
        if image.width <= PROJECTED_MAX_SIDE && image.height <= PROJECTED_MAX_SIDE {
            return Self::image(image);
        }
        let dimensions = (image.width, image.height);
        // raw 不放入 previews；只有有界投影帧会进入底图绘制，原图保留 CPU 像素合同。
        let raw = render_image(image.width, image.height, image.bgra);
        Self::shared_image(raw, dimensions)
    }
    /// 共享原始 CPU 帧并用棋盘显示透明区；长图只生成有界预览，导出仍读取原图。
    pub(crate) fn shared_image(
        raw: Arc<RenderImage>,
        dimensions: (u32, u32),
    ) -> Result<(Self, Vec<Arc<RenderImage>>)> {
        Self::shared_image_preview(raw, dimensions, false)
    }
    /// 贴图显示真实透明像素；编辑器仍使用棋盘预览，二者不改原图。
    pub(crate) fn shared_pin_image(
        raw: Arc<RenderImage>,
        dimensions: (u32, u32),
    ) -> Result<(Self, Vec<Arc<RenderImage>>)> {
        Self::shared_image_preview(raw, dimensions, true)
    }
    fn shared_image_preview(
        raw: Arc<RenderImage>,
        dimensions: (u32, u32),
        preserve_alpha: bool,
    ) -> Result<(Self, Vec<Arc<RenderImage>>)> {
        let (width, height) = dimensions;
        let bytes = raw.as_bytes(0).ok_or("图片没有原始像素帧")?;
        let actual = raw.size(0);
        if width == 0
            || height == 0
            || width > MAX_IMAGE_SIDE
            || height > MAX_IMAGE_SIDE
            || width as u64 * height as u64 > 32_000_000
            || bytes.len() != width as usize * height as usize * 4
            || u32::from(actual.width) != width
            || u32::from(actual.height) != height
        {
            return Err(tr!(
                "图片尺寸无效或超过 3200 万像素 / 65535 单边限制",
                "Invalid image size, or over 32 megapixels / 65535 px per side"
            )
            .into());
        }
        if width > 16384 || height > 16384 {
            let region = Rect::full(width, height);
            let density = f64::from(PROJECTED_INITIAL_SIDE) / f64::from(width.max(height));
            let grid = projection_grid(region, density)?;
            let preview = render_image(
                grid.pixel_bounds.width,
                grid.pixel_bounds.height,
                projected_grid_pixels_with_alpha(dimensions, bytes, grid, preserve_alpha)?,
            );
            return Ok((
                Self {
                    bounds: ScreenRect {
                        left: 0,
                        top: 0,
                        width,
                        height,
                    },
                    current: 0,
                    capture_ms: 0.,
                    backend: Backend::ProjectedImage {
                        raw,
                        region,
                        grid,
                        preserve_alpha,
                    },
                },
                vec![preview],
            ));
        }
        let preview = if !preserve_alpha && bytes.chunks_exact(4).any(|p| p[3] != 255) {
            render_image(width, height, checker_pixels(width, bytes, (0, 0)))
        } else {
            raw.clone()
        };
        Ok((
            Self {
                bounds: ScreenRect {
                    left: 0,
                    top: 0,
                    width,
                    height,
                },
                current: 0,
                capture_ms: 0.,
                backend: if preserve_alpha {
                    Backend::PresentationImage(raw)
                } else {
                    Backend::Image(raw)
                },
            },
            vec![preview],
        ))
    }
    /// 截图外观专用共享入口：原图只验证长度与尺寸，不扫描 alpha、不生成棋盘或 Box。
    /// 长图预览只放 1×1 透明占位；源采样始终通过 original/full stride 读取 raw。
    pub(crate) fn shared_presentation_image(
        raw: Arc<RenderImage>,
        dimensions: (u32, u32),
    ) -> Result<(Self, Vec<Arc<RenderImage>>)> {
        let (width, height) = dimensions;
        let bytes = raw.as_bytes(0).ok_or("图片没有原始像素帧")?;
        let actual = raw.size(0);
        if width == 0
            || height == 0
            || width > MAX_IMAGE_SIDE
            || height > MAX_IMAGE_SIDE
            || u64::from(width) * u64::from(height) > 32_000_000
            || bytes.len() != width as usize * height as usize * 4
            || u32::from(actual.width) != width
            || u32::from(actual.height) != height
        {
            return Err(tr!(
                "图片尺寸无效或超过 3200 万像素 / 65535 单边限制",
                "Invalid image size, or over 32 megapixels / 65535 px per side"
            )
            .into());
        }
        let (backend, preview) = if width > 16384 || height > 16384 {
            (
                Backend::PresentationImage(raw),
                render_image(1, 1, vec![0; 4]),
            )
        } else {
            (Backend::PresentationImage(raw.clone()), raw)
        };
        Ok((
            Self {
                bounds: ScreenRect {
                    left: 0,
                    top: 0,
                    width,
                    height,
                },
                current: 0,
                capture_ms: 0.,
                backend,
            },
            vec![preview],
        ))
    }
    /// 只更新长图贴图的显示分辨率；返回新图后，由宿主退休上一张预览。
    /// 宿主尺寸包装用于初始化/测试；生产提供精确 density，避免整数宿主尺寸重算缩放相位。
    #[cfg(test)]
    pub(crate) fn refresh_shared_preview(
        &mut self,
        region: Rect,
        host: (u32, u32),
    ) -> Result<Option<Arc<RenderImage>>> {
        if host.0 == 0 || host.1 == 0 {
            return Ok(None);
        }
        if region.width == 0 || region.height == 0 {
            return Err("贴图显示预览范围无效".into());
        }
        let density = (f64::from(host.0) / f64::from(region.width))
            .min(f64::from(host.1) / f64::from(region.height));
        self.refresh_shared_preview_density(region, density)
    }
    /// 所有目标像素都锚定全图原点，不随可见 ROI 的起点/尺寸重新铺采样栅格。
    pub(crate) fn refresh_shared_preview_at(
        &mut self,
        region: Rect,
        density: f32,
    ) -> Result<Option<Arc<RenderImage>>> {
        self.refresh_shared_preview_density(region, f64::from(density))
    }
    fn refresh_shared_preview_density(
        &mut self,
        region: Rect,
        density: f64,
    ) -> Result<Option<Arc<RenderImage>>> {
        let Backend::ProjectedImage {
            raw,
            region: current_region,
            grid: current_grid,
            preserve_alpha,
        } = &mut self.backend
        else {
            return Ok(None);
        };
        let actual = raw.size(0);
        let dimensions = (u32::from(actual.width), u32::from(actual.height));
        if !region.valid(dimensions.0, dimensions.1) {
            return Err("贴图显示预览范围超出原图".into());
        }
        let grid = projection_grid(region, density)?;
        if grid == *current_grid {
            *current_region = region;
            return Ok(None);
        }
        let bytes = raw.as_bytes(0).ok_or("图片没有原始像素帧")?;
        let preview = render_image(
            grid.pixel_bounds.width,
            grid.pixel_bounds.height,
            projected_grid_pixels_with_alpha(dimensions, bytes, grid, *preserve_alpha)?,
        );
        // 分配和采样成功后才推进 key，失败不会把旧图误认作新尺寸。
        *current_region = region;
        *current_grid = grid;
        Ok(Some(preview))
    }
    fn tiled_image(width: u32, height: u32, bytes: &[u8]) -> Result<(Self, Vec<Arc<RenderImage>>)> {
        let mut tiles = Vec::new();
        for y in (0..height).step_by(IMAGE_TILE_SIDE as usize) {
            for x in (0..width).step_by(IMAGE_TILE_SIDE as usize) {
                let rect = Rect {
                    x,
                    y,
                    width: IMAGE_TILE_SIDE.min(width - x),
                    height: IMAGE_TILE_SIDE.min(height - y),
                };
                let mut tile_bytes = Vec::new();
                tile_bytes.try_reserve_exact(rect.width as usize * rect.height as usize * 4)?;
                for row in y..y + rect.height {
                    let start = (row as usize * width as usize + x as usize) * 4;
                    tile_bytes.extend_from_slice(&bytes[start..start + rect.width as usize * 4]);
                }
                let matte = tile_bytes
                    .chunks_exact(4)
                    .any(|p| p[3] != 255)
                    .then(|| checker_pixels(rect.width, &tile_bytes, (x, y)));
                let original = render_image(rect.width, rect.height, tile_bytes);
                let preview = matte.map_or_else(
                    || original.clone(),
                    |bgra| render_image(rect.width, rect.height, bgra),
                );
                tiles.push(ImageTile {
                    rect,
                    original,
                    preview,
                });
            }
        }
        let previews = tiles.iter().map(|t| t.preview.clone()).collect();
        Ok((
            Self {
                bounds: ScreenRect {
                    left: 0,
                    top: 0,
                    width,
                    height,
                },
                current: 0,
                capture_ms: 0.,
                backend: Backend::ImageTiles(tiles),
            },
            previews,
        ))
    }
    pub fn is_image(&self) -> bool {
        matches!(
            self.backend,
            Backend::Image(_)
                | Backend::PresentationImage(_)
                | Backend::ImageTiles(_)
                | Backend::ProjectedImage { .. }
                | Backend::Solid(_)
                | Backend::CaptureContext { .. }
        )
    }
    pub(crate) fn is_projected_image(&self) -> bool {
        matches!(self.backend, Backend::ProjectedImage { .. })
            || (matches!(self.backend, Backend::PresentationImage(_))
                && (self.bounds.width > 16384 || self.bounds.height > 16384))
    }
    pub fn screen_count(&self) -> usize {
        match &self.backend {
            Backend::Desktop(d) => d.screens.len(),
            Backend::FrozenDesktop(d) => d.screens.len(),
            Backend::Image(_)
            | Backend::PresentationImage(_)
            | Backend::ProjectedImage { .. }
            | Backend::Solid(_) => 1,
            Backend::CaptureContext { .. } => 1,
            Backend::ImageTiles(tiles) => tiles.len(),
        }
    }
    pub fn full(&self) -> Rect {
        Rect::full(self.bounds.width, self.bounds.height)
    }
    pub fn screen_rect(&self, index: usize) -> Rect {
        match &self.backend {
            Backend::Desktop(d) => d.screen_rect(index),
            Backend::FrozenDesktop(d) => d.screen_rect(index),
            Backend::Image(_) | Backend::PresentationImage(_) | Backend::Solid(_) => self.full(),
            Backend::ImageTiles(tiles) => tiles[index].rect,
            Backend::ProjectedImage { region, .. } => *region,
            Backend::CaptureContext { content, .. } => *content,
        }
    }
    /// 显示 ROI 与原始采样平面分离，Scene 不能把小预览范围当作原始 BGRA 的 stride。
    pub(crate) fn original_rect(&self, index: usize) -> Rect {
        match &self.backend {
            Backend::ProjectedImage { .. } => self.full(),
            _ => self.screen_rect(index),
        }
    }
    /// 显示范围可含边界采样格的小数扩展；绘制时另用原图 full 边界裁剪。
    pub(crate) fn preview_bounds(&self, index: usize) -> Bounds {
        if let Backend::ProjectedImage { grid, .. } = &self.backend {
            Bounds {
                x: (f64::from(grid.pixel_bounds.x) / grid.density) as f32,
                y: (f64::from(grid.pixel_bounds.y) / grid.density) as f32,
                width: (f64::from(grid.pixel_bounds.width) / grid.density) as f32,
                height: (f64::from(grid.pixel_bounds.height) / grid.density) as f32,
            }
        } else {
            let rect = self.screen_rect(index);
            Bounds {
                x: rect.x as f32,
                y: rect.y as f32,
                width: rect.width as f32,
                height: rect.height as f32,
            }
        }
    }
    /// QA 只借用当前原图与栅格，不增加强引用或估算原始 Vec capacity。
    pub(crate) fn projected_image_for_qa(&self) -> Option<(&Arc<RenderImage>, Rect, Rect, f64)> {
        match &self.backend {
            Backend::ProjectedImage {
                raw, region, grid, ..
            } => Some((raw, *region, grid.pixel_bounds, grid.density)),
            _ => None,
        }
    }
    pub fn monitor_at(&self, point: (i32, i32)) -> usize {
        match &self.backend {
            Backend::Desktop(d) => d.monitor_at(point),
            Backend::FrozenDesktop(d) => d.monitor_at(point),
            Backend::Image(_)
            | Backend::PresentationImage(_)
            | Backend::ImageTiles(_)
            | Backend::ProjectedImage { .. }
            | Backend::Solid(_) => 0,
            Backend::CaptureContext { .. } => 0,
        }
    }
    pub fn original<'a>(&'a self, preview: &'a Arc<RenderImage>) -> &'a RenderImage {
        match &self.backend {
            Backend::Desktop(_) | Backend::FrozenDesktop(_) | Backend::CaptureContext { .. } => {
                preview
            }
            Backend::Image(raw)
            | Backend::PresentationImage(raw)
            | Backend::ProjectedImage { raw, .. } => raw,
            Backend::ImageTiles(tiles) => tiles
                .iter()
                .find(|t| Arc::ptr_eq(&t.preview, preview))
                .map(|t| t.original.as_ref())
                .expect("preview belongs to image tiles"),
            // 纯色底不产生预览，调用方只会遍历空的 previews。
            Backend::Solid(_) => preview,
        }
    }
    /// 原图强引用只用于已共享 CPU 帧的贴图；不生成棋盘格或读取桌面。
    /// 调用方仍须遵守 GPU 单边上限，ProjectedImage 只交有界目标块上传。
    #[cfg(test)]
    pub(crate) fn shared_original(&self) -> Option<Arc<RenderImage>> {
        match &self.backend {
            Backend::Image(raw)
            | Backend::PresentationImage(raw)
            | Backend::ProjectedImage { raw, .. } => Some(raw.clone()),
            _ => None,
        }
    }
    pub fn read_region(&self, rect: Rect) -> Result<Image> {
        if !rect.valid(self.bounds.width, self.bounds.height) {
            return Err("裁剪范围超出原图".into());
        }
        match &self.backend {
            Backend::Desktop(d) => d.read_region(rect),
            Backend::FrozenDesktop(d) => d.read_region(rect),
            Backend::Image(raw)
            | Backend::PresentationImage(raw)
            | Backend::ProjectedImage { raw, .. } => Ok(SourcePixels {
                fill: None,
                canvas: self.full(),
                overlay: None,
                planes: vec![PixelPlane {
                    rect: self.full(),
                    bgra: raw.as_bytes(0).expect("original frame"),
                }],
            }
            .read(rect)),
            Backend::ImageTiles(tiles) => Ok(SourcePixels {
                fill: None,
                canvas: self.full(),
                overlay: None,
                planes: tiles
                    .iter()
                    .map(|tile| PixelPlane {
                        rect: tile.rect,
                        bgra: tile.original.as_bytes(0).expect("original tile"),
                    })
                    .collect(),
            }
            .read(rect)),
            Backend::Solid(bgra) => Ok(SourcePixels {
                fill: Some(*bgra),
                canvas: self.full(),
                overlay: None,
                planes: Vec::new(),
            }
            .read(rect)),
            Backend::CaptureContext { pixels, .. } => pixels.read_content(rect),
        }
    }
    pub fn restore_preview(&self, image: &Image, rect: Rect) -> Vec<u8> {
        if matches!(
            self.backend,
            Backend::PresentationImage(_)
                | Backend::CaptureContext { .. }
                | Backend::ProjectedImage {
                    preserve_alpha: true,
                    ..
                }
        ) {
            // 贴图全部内容与 effects 共用真实 alpha，不在局部缓存中烘焙棋盘。
            image.bgra.clone()
        } else if self.is_image() {
            checker_preview(image, (rect.x, rect.y))
        } else {
            image
                .bgra
                .chunks_exact(4)
                .flat_map(|p| {
                    let alpha = p[3] as u32;
                    [
                        (p[0] as u32 * alpha / 255) as u8,
                        (p[1] as u32 * alpha / 255) as u8,
                        (p[2] as u32 * alpha / 255) as u8,
                        255,
                    ]
                })
                .collect()
        }
    }
}
fn projection_grid(region: Rect, density: f64) -> Result<ProjectionGrid> {
    let right = region
        .x
        .checked_add(region.width)
        .ok_or("贴图预览范围溢出")?;
    let bottom = region
        .y
        .checked_add(region.height)
        .ok_or("贴图预览范围溢出")?;
    if region.width == 0 || region.height == 0 || !density.is_finite() || density <= 0. {
        return Err("贴图预览范围或像素密度无效".into());
    }
    let mut density = density.min(1.);
    for _ in 0..32 {
        let x = (f64::from(region.x) * density).floor() as u32;
        let y = (f64::from(region.y) * density).floor() as u32;
        let pixel_right = (f64::from(right) * density).ceil() as u32;
        let pixel_bottom = (f64::from(bottom) * density).ceil() as u32;
        let width = pixel_right.checked_sub(x).ok_or("贴图预览栅格溢出")?;
        let height = pixel_bottom.checked_sub(y).ok_or("贴图预览栅格溢出")?;
        if width == 0 || height == 0 {
            return Err("贴图预览栅格为空".into());
        }
        let pixels = u64::from(width) * u64::from(height);
        if width <= PROJECTED_MAX_SIDE
            && height <= PROJECTED_MAX_SIDE
            && pixels <= PROJECTED_MAX_PIXELS
        {
            if f64::from(pixel_right) / density > f64::from(f32::MAX)
                || f64::from(pixel_bottom) / density > f64::from(f32::MAX)
            {
                return Err("贴图预览坐标超出浮点显示范围".into());
            }
            return Ok(ProjectionGrid {
                density,
                pixel_bounds: Rect {
                    x,
                    y,
                    width,
                    height,
                },
            });
        }
        // 只在实际输出超出预算时统一降低密度；普通可见区域保持真实物理密度。
        let reduction = (f64::from(PROJECTED_MAX_SIDE) / f64::from(width))
            .min(f64::from(PROJECTED_MAX_SIDE) / f64::from(height))
            .min((PROJECTED_MAX_PIXELS as f64 / pixels as f64).sqrt());
        density *= reduction * 0.999_999;
    }
    Err("贴图预览栅格未收敛到输出预算".into())
}

#[cfg(test)]
fn projected_size(dimensions: (u32, u32), host: (u32, u32)) -> Option<(u32, u32)> {
    let (width, height) = dimensions;
    if width == 0 || height == 0 || host.0 == 0 || host.1 == 0 {
        return None;
    }
    let pixels = u64::from(width) * u64::from(height);
    let scale = 1_f64
        .min(f64::from(host.0) / f64::from(width))
        .min(f64::from(host.1) / f64::from(height))
        .min(f64::from(PROJECTED_MAX_SIDE) / f64::from(width.max(height)))
        .min((PROJECTED_MAX_PIXELS as f64 / pixels as f64).sqrt());
    // 窄长图至少保留一列/行；其余尺寸向下取整，不超过宿主或输出预算。
    Some((
        (f64::from(width) * scale).floor().max(1.) as u32,
        (f64::from(height) * scale).floor().max(1.) as u32,
    ))
}

/// 面积加权 Box 缩小：目标格从整张原图的固定物理网格反投影，只裁到源边界。
/// 只分配输出 Vec，不创建完整浮点图、通道转换图或逐行累计缓冲。
#[cfg(test)]
fn projected_grid_pixels(
    dimensions: (u32, u32),
    bgra: &[u8],
    grid: ProjectionGrid,
) -> Result<Vec<u8>> {
    projected_grid_pixels_with_alpha(dimensions, bgra, grid, false)
}
fn projected_grid_pixels_with_alpha(
    dimensions: (u32, u32),
    bgra: &[u8],
    grid: ProjectionGrid,
    preserve_alpha: bool,
) -> Result<Vec<u8>> {
    let (width, height) = dimensions;
    let output_width = grid.pixel_bounds.width;
    let output_height = grid.pixel_bounds.height;
    let source_pixels = u64::from(width) * u64::from(height);
    let output_pixels = u64::from(output_width) * u64::from(output_height);
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("贴图预览源尺寸溢出")?;
    if width == 0
        || height == 0
        || width > MAX_IMAGE_SIDE
        || height > MAX_IMAGE_SIDE
        || source_pixels > 32_000_000
        || bgra.len() != expected
        || output_width == 0
        || output_height == 0
        || !grid.density.is_finite()
        || grid.density <= 0.
        || grid.density > 1.
        || output_width > PROJECTED_MAX_SIDE
        || output_height > PROJECTED_MAX_SIDE
        || output_pixels > PROJECTED_MAX_PIXELS
    {
        return Err("贴图显示预览尺寸或像素缓冲无效".into());
    }
    let pixel_right = grid
        .pixel_bounds
        .x
        .checked_add(output_width)
        .ok_or("贴图预览栅格溢出")?;
    let pixel_bottom = grid
        .pixel_bounds
        .y
        .checked_add(output_height)
        .ok_or("贴图预览栅格溢出")?;
    let sample_left = (f64::from(grid.pixel_bounds.x) / grid.density)
        .floor()
        .min(f64::from(width)) as u32;
    let sample_top = (f64::from(grid.pixel_bounds.y) / grid.density)
        .floor()
        .min(f64::from(height)) as u32;
    let sample_right = (f64::from(pixel_right) / grid.density)
        .ceil()
        .min(f64::from(width)) as u32;
    let sample_bottom = (f64::from(pixel_bottom) / grid.density)
        .ceil()
        .min(f64::from(height)) as u32;
    if sample_right <= sample_left || sample_bottom <= sample_top {
        return Err("贴图预览栅格超出原图".into());
    }
    let output_bytes = (output_width as usize)
        .checked_mul(output_height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("贴图显示预览尺寸溢出")?;
    let mut output = Vec::new();
    output.try_reserve_exact(output_bytes)?;
    // 扫描实际采样格覆盖范围，边缘格会读取请求 ROI 外的少量像素。
    let opaque = !preserve_alpha
        && (sample_top..sample_bottom).all(|y| {
            let start = ((u64::from(y) * u64::from(width) + u64::from(sample_left)) * 4) as usize;
            bgra[start..start + (sample_right - sample_left) as usize * 4]
                .chunks_exact(4)
                .all(|pixel| pixel[3] == 255)
        });
    for y in 0..output_height {
        let global_y = grid.pixel_bounds.y + y;
        let top = (f64::from(global_y) / grid.density).min(f64::from(height));
        let bottom = (f64::from(global_y + 1) / grid.density).min(f64::from(height));
        let first_y = top.floor() as u32;
        let last_y = bottom.ceil().min(f64::from(height)) as u32;
        for x in 0..output_width {
            let global_x = grid.pixel_bounds.x + x;
            let left = (f64::from(global_x) / grid.density).min(f64::from(width));
            let right = (f64::from(global_x + 1) / grid.density).min(f64::from(width));
            let first_x = left.floor() as u32;
            let last_x = right.ceil().min(f64::from(width)) as u32;
            let area = (right - left) * (bottom - top);
            if area <= 0. {
                return Err("贴图预览采样格为空".into());
            }
            let mut sums = [0_f64; 3];
            let mut alpha_sum = 0_f64;
            for source_y in first_y..last_y {
                let vertical = bottom.min(f64::from(source_y + 1)) - top.max(f64::from(source_y));
                for source_x in first_x..last_x {
                    let horizontal =
                        right.min(f64::from(source_x + 1)) - left.max(f64::from(source_x));
                    let weight = horizontal * vertical;
                    let at = ((u64::from(source_y) * u64::from(width) + u64::from(source_x)) * 4)
                        as usize;
                    let pixel = &bgra[at..at + 4];
                    if preserve_alpha {
                        // 先累计预乘颜色，再恢复 straight BGRA，透明像素的隐藏 RGB 不参与采样。
                        let alpha_weight = f64::from(pixel[3]) * weight;
                        alpha_sum += alpha_weight;
                        for channel in 0..3 {
                            sums[channel] += f64::from(pixel[channel]) * alpha_weight;
                        }
                    } else if opaque || pixel[3] == 255 {
                        for channel in 0..3 {
                            sums[channel] += f64::from(pixel[channel]) * weight;
                        }
                    } else {
                        // 棋盘始终按原图坐标混合，再做面积平均，缩放不改变棋盘相位。
                        let background = if (source_x / 16 + source_y / 16).is_multiple_of(2) {
                            224_u32
                        } else {
                            192
                        };
                        let alpha = u32::from(pixel[3]);
                        for channel in 0..3 {
                            let blended = (u32::from(pixel[channel]) * alpha
                                + background * (255 - alpha)
                                + 127)
                                / 255;
                            sums[channel] += f64::from(blended) * weight;
                        }
                    }
                }
            }
            if preserve_alpha {
                let alpha = (alpha_sum / area).round().clamp(0., 255.) as u8;
                if alpha == 0 {
                    output.extend_from_slice(&[0; 4]);
                } else {
                    output.extend_from_slice(&[
                        (sums[0] / alpha_sum).round().clamp(0., 255.) as u8,
                        (sums[1] / alpha_sum).round().clamp(0., 255.) as u8,
                        (sums[2] / alpha_sum).round().clamp(0., 255.) as u8,
                        alpha,
                    ]);
                }
                continue;
            }
            // f64 权重包含小数边缘，相同 global 格与 ROI 无关，累加不会发生窄图整数溢出。
            output.extend_from_slice(&[
                (sums[0] / area).round().clamp(0., 255.) as u8,
                (sums[1] / area).round().clamp(0., 255.) as u8,
                (sums[2] / area).round().clamp(0., 255.) as u8,
                255,
            ]);
        }
    }
    Ok(output)
}

#[cfg(test)]
fn projected_pixels(
    dimensions: (u32, u32),
    bgra: &[u8],
    region: Rect,
    target: (u32, u32),
) -> Result<Vec<u8>> {
    if !region.valid(dimensions.0, dimensions.1)
        || target.0 == 0
        || target.1 == 0
        || target.0 > region.width
        || target.1 > region.height
    {
        return Err("贴图显示预览尺寸或范围无效".into());
    }
    let density = (f64::from(target.0) / f64::from(region.width))
        .min(f64::from(target.1) / f64::from(region.height));
    projected_grid_pixels(dimensions, bgra, projection_grid(region, density)?)
}
pub(crate) fn render_image(width: u32, height: u32, bgra: Vec<u8>) -> Arc<RenderImage> {
    Arc::new(RenderImage::new(vec![image::Frame::new(
        image::RgbaImage::from_raw(width, height, bgra).expect("validated dimensions"),
    )]))
}
// 棋盘格仅用于显示，按原图坐标对齐；擦除局部图层使用同一原点，导出仍保持直 alpha。
fn checker_preview(image: &Image, origin: (u32, u32)) -> Vec<u8> {
    checker_pixels(image.width, &image.bgra, origin)
}
fn checker_pixels(width: u32, bgra: &[u8], origin: (u32, u32)) -> Vec<u8> {
    bgra.chunks_exact(4)
        .enumerate()
        .flat_map(|(i, p)| {
            let x = origin.0 + i as u32 % width;
            let y = origin.1 + i as u32 / width;
            let background = if (x / 16 + y / 16).is_multiple_of(2) {
                224_u32
            } else {
                192
            };
            let alpha = p[3] as u32;
            let blend = |c: u8| ((c as u32 * alpha + background * (255 - alpha) + 127) / 255) as u8;
            [blend(p[0]), blend(p[1]), blend(p[2]), 255]
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_shared_preview_and_restore_keep_alpha_without_checker() {
        let bytes = vec![240, 5, 210, 0, 20, 60, 100, 128, 3, 5, 7, 255];
        let raw = render_image(3, 1, bytes.clone());
        let (source, previews) = CanvasSource::shared_pin_image(raw.clone(), (3, 1)).unwrap();
        assert!(Arc::ptr_eq(&raw, &previews[0]));
        assert_eq!(previews[0].as_bytes(0).unwrap(), bytes);
        let cropped = source.read_region(Rect::full(2, 1)).unwrap();
        assert_eq!(
            source.restore_preview(&cropped, Rect::full(2, 1)),
            bytes[..8]
        );
        // 同一原图进独立编辑器仍显示棋盘；显示策略不能污染权威像素。
        let (editor, editor_previews) = CanvasSource::shared_image(raw, (3, 1)).unwrap();
        assert_eq!(
            &editor_previews[0].as_bytes(0).unwrap()[..4],
            &[224, 224, 224, 255]
        );
        assert_eq!(editor.read_region(editor.full()).unwrap().bgra, bytes);
    }

    #[test]
    fn pin_projected_preview_averages_premultiplied_alpha_without_hidden_rgb() {
        let bytes = [
            240, 5, 210, 0, 20, 60, 100, 128, 250, 2, 150, 0, 20, 250, 180, 0,
        ];
        let preview = projected_grid_pixels_with_alpha(
            (4, 1),
            &bytes,
            projection_grid(Rect::full(4, 1), 0.5).unwrap(),
            true,
        )
        .unwrap();
        assert_eq!(preview, [20, 60, 100, 64, 0, 0, 0, 0]);
    }

    #[test]
    fn pin_shared_long_preview_preserves_alpha_after_refresh_and_restore() {
        let (width, height) = (32, 17_000);
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            let pixel = if y < 5_000 {
                [240, 5, 210, 0]
            } else if y < 12_000 {
                [20, 60, 100, 128]
            } else {
                [3, 5, 7, 255]
            };
            for _ in 0..width {
                bytes.extend_from_slice(&pixel);
            }
        }
        let raw = render_image(width, height, bytes);
        let (mut source, previews) =
            CanvasSource::shared_pin_image(raw.clone(), (width, height)).unwrap();
        let initial = previews[0].as_bytes(0).unwrap();
        for alpha in [0, 128, 255] {
            assert!(initial.chunks_exact(4).any(|p| p[3] == alpha));
        }
        assert!(initial
            .chunks_exact(4)
            .filter(|p| p[3] == 0)
            .all(|p| p == [0; 4]));
        let region = Rect {
            x: 0,
            y: 4_998,
            width,
            height: 8,
        };
        let preview = source
            .refresh_shared_preview_at(region, 0.5)
            .unwrap()
            .unwrap();
        let preview = preview.as_bytes(0).unwrap();
        assert_eq!(&preview[..4], &[0; 4]);
        assert_eq!(&preview[16 * 4..16 * 4 + 4], &[20, 60, 100, 128]);
        assert!(source
            .refresh_shared_preview_at(region, 0.5)
            .unwrap()
            .is_none());
        let cropped = source.read_region(region).unwrap();
        assert_eq!(source.restore_preview(&cropped, region), cropped.bgra);
        assert!(std::ptr::eq(source.original(&previews[0]), raw.as_ref()));
    }
    #[test]
    fn long_image_editor_projected_ownership_preserves_raw_and_avoids_tiles() {
        fn pixel(x: u32, y: u32) -> [u8; 4] {
            [
                ((x * 17 + y * 13) % 251) as u8,
                ((x * 7 + y * 23) % 251) as u8,
                ((x * 19 + y * 3) % 251) as u8,
                [0, 128, 255][((x + y) % 3) as usize],
            ]
        }
        for (width, height) in [(33, 17_017), (17_017, 35)] {
            let mut bytes = Vec::with_capacity(width as usize * height as usize * 4);
            for y in 0..height {
                for x in 0..width {
                    bytes.extend_from_slice(&pixel(x, y));
                }
            }
            let original_pointer = bytes.as_ptr();
            let original_capacity = bytes.capacity();
            let (source, previews) = CanvasSource::editor_image(Image {
                width,
                height,
                bgra: bytes,
                readback_ms: 0.,
            })
            .unwrap();
            let Backend::ProjectedImage { raw, .. } = &source.backend else {
                panic!("editor long image must retain one raw frame and a projected preview");
            };
            assert_eq!(previews.len(), 1);
            assert!(source.is_projected_image());
            assert_eq!(source.screen_count(), 1);
            assert_eq!(source.full(), Rect::full(width, height));
            assert_eq!(source.original_rect(0), source.full());
            assert_eq!(source.screen_rect(0), source.full());
            assert_eq!(source.current, 0);
            assert_eq!(source.monitor_at((width as i32 - 1, height as i32 - 1)), 0);
            assert!(std::ptr::eq(source.original(&previews[0]), raw.as_ref()));
            assert!(!Arc::ptr_eq(raw, &previews[0]));
            let original = raw.as_bytes(0).unwrap();
            assert_eq!(original.as_ptr(), original_pointer);
            assert_eq!(original.len(), width as usize * height as usize * 4);
            assert_eq!(
                (u32::from(raw.size(0).width), u32::from(raw.size(0).height)),
                (width, height)
            );
            let preview_size = previews[0].size(0);
            assert!(u32::from(preview_size.width) <= PROJECTED_INITIAL_SIDE);
            assert!(u32::from(preview_size.height) <= PROJECTED_INITIAL_SIDE);
            assert!(previews[0]
                .as_bytes(0)
                .unwrap()
                .chunks_exact(4)
                .all(|p| p[3] == 255));
            for (x, y) in [(0, 0), (17, 0), (width - 1, height - 1)] {
                let at = ((y * width + x) * 4) as usize;
                assert_eq!(&original[at..at + 4], &pixel(x, y));
            }
            for boundary in [4096, 8192] {
                let rect = if width > PROJECTED_MAX_SIDE {
                    Rect {
                        x: boundary - 3,
                        y: 2,
                        width: 7,
                        height: 29,
                    }
                } else {
                    Rect {
                        x: 2,
                        y: boundary - 3,
                        width: 29,
                        height: 7,
                    }
                };
                let cropped = source.read_region(rect).unwrap();
                for y in 0..rect.height {
                    for x in 0..rect.width {
                        let at = ((y * rect.width + x) * 4) as usize;
                        assert_eq!(&cropped.bgra[at..at + 4], &pixel(rect.x + x, rect.y + y));
                    }
                }
            }
            let tail = source
                .read_region(Rect {
                    x: width - 1,
                    y: height - 1,
                    width: 1,
                    height: 1,
                })
                .unwrap();
            assert_eq!(tail.bgra, pixel(width - 1, height - 1));
            let (qa_raw, qa_region, qa_grid, qa_density) = source.projected_image_for_qa().unwrap();
            assert!(Arc::ptr_eq(qa_raw, raw));
            assert_eq!(qa_region, source.full());
            assert_eq!(
                (qa_grid.width, qa_grid.height),
                (
                    u32::from(preview_size.width),
                    u32::from(preview_size.height)
                )
            );
            assert!(qa_density > 0. && qa_density <= 1.);
            drop(previews);
            let Backend::ProjectedImage { raw, .. } = source.backend else {
                unreachable!()
            };
            let raw = match Arc::try_unwrap(raw) {
                Ok(raw) => raw,
                Err(_) => panic!("CPU original must have no retained strong alias"),
            };
            let frame = match raw.try_into_single_frame() {
                Ok(frame) => frame,
                Err(_) => panic!("factory must preserve one unscaled raw frame"),
            };
            assert_eq!(frame.buffer().as_raw().as_ptr(), original_pointer);
            assert_eq!(frame.buffer().as_raw().capacity(), original_capacity);
        }
        // 小图和16384边界继续使用原工厂；透明棋盘不进入原始像素。
        let transparent = vec![20, 40, 80, 0, 12, 24, 80, 128, 3, 5, 7, 255];
        let pointer = transparent.as_ptr();
        let (source, previews) = CanvasSource::editor_image(Image {
            width: 3,
            height: 1,
            bgra: transparent,
            readback_ms: 0.,
        })
        .unwrap();
        assert!(matches!(source.backend, Backend::Image(_)));
        assert!(source.projected_image_for_qa().is_none());
        assert!(!source.is_projected_image());
        assert_eq!(
            source.original(&previews[0]).as_bytes(0).unwrap().as_ptr(),
            pointer
        );
        assert_eq!(
            source.read_region(source.full()).unwrap().bgra,
            [20, 40, 80, 0, 12, 24, 80, 128, 3, 5, 7, 255]
        );
        assert_eq!(
            previews[0].as_bytes(0).unwrap(),
            [224, 224, 224, 255, 118, 124, 152, 255, 3, 5, 7, 255]
        );
        let (source, previews) = CanvasSource::editor_image(Image {
            width: 1,
            height: 16384,
            bgra: [3, 5, 7, 255].repeat(16384),
            readback_ms: 0.,
        })
        .unwrap();
        assert!(matches!(source.backend, Backend::Image(_)));
        assert!(std::ptr::eq(
            source.original(&previews[0]),
            previews[0].as_ref()
        ));
        for (width, height, bgra) in [
            (0, 1, Vec::new()),
            (65536, 1, Vec::new()),
            (2000, 17000, Vec::new()),
            (33, 17017, vec![0; 7]),
        ] {
            assert!(CanvasSource::editor_image(Image {
                width,
                height,
                bgra,
                readback_ms: 0.
            })
            .is_err());
        }
    }
    #[test]
    fn projected_shared_keeps_raw_alpha_and_cross_4096_crops() {
        let (width, height) = (33, 17_017);
        let mut bytes = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                bytes.extend_from_slice(&[
                    ((x * 17 + y * 13) % 251) as u8,
                    ((x * 7 + y * 23) % 251) as u8,
                    ((x * 19 + y * 3) % 251) as u8,
                    [0, 128, 255][((x + y) % 3) as usize],
                ]);
            }
        }
        let raw = render_image(width, height, bytes);
        let (mut source, previews) =
            CanvasSource::shared_image(raw.clone(), (width, height)).unwrap();
        let Backend::ProjectedImage {
            raw: stored, grid, ..
        } = &source.backend
        else {
            panic!("shared long image must use a projected preview");
        };
        assert!(Arc::ptr_eq(stored, &raw));
        assert_eq!(
            (grid.pixel_bounds.width, grid.pixel_bounds.height),
            (2, 1024)
        );
        assert_eq!(source.screen_count(), 1);
        assert_eq!(source.screen_rect(0), Rect::full(width, height));
        assert_eq!(source.monitor_at((32, 17_016)), 0);
        assert!(std::ptr::eq(source.original(&previews[0]), raw.as_ref()));
        assert!(!Arc::ptr_eq(&raw, &previews[0]));
        assert_eq!(previews[0].as_bytes(0).unwrap().len(), 2 * 1024 * 4);
        assert!(previews[0]
            .as_bytes(0)
            .unwrap()
            .chunks_exact(4)
            .all(|p| p[3] == 255));
        let rect = Rect {
            x: 2,
            y: 4093,
            width: 29,
            height: 7,
        };
        let cropped = source.read_region(rect).unwrap();
        for y in 0..rect.height {
            for x in 0..rect.width {
                let gx = rect.x + x;
                let gy = rect.y + y;
                let at = ((y * rect.width + x) * 4) as usize;
                assert_eq!(
                    &cropped.bgra[at..at + 4],
                    &[
                        ((gx * 17 + gy * 13) % 251) as u8,
                        ((gx * 7 + gy * 23) % 251) as u8,
                        ((gx * 19 + gy * 3) % 251) as u8,
                        [0, 128, 255][((gx + gy) % 3) as usize],
                    ]
                );
            }
        }
        let roi_preview = source
            .refresh_shared_preview(rect, (29, 7))
            .unwrap()
            .unwrap();
        assert_eq!(source.screen_rect(0), rect);
        assert_eq!(source.original_rect(0), Rect::full(width, height));
        assert_eq!(
            roi_preview.as_bytes(0).unwrap(),
            source.restore_preview(&cropped, rect)
        );
        assert!(source
            .refresh_shared_preview(rect, (29, 7))
            .unwrap()
            .is_none());
        let moved = Rect { y: 4094, ..rect };
        assert!(source
            .refresh_shared_preview(moved, (29, 7))
            .unwrap()
            .is_some());
        assert_eq!(source.screen_rect(0), moved);
        assert!(source
            .refresh_shared_preview(Rect { x: width, ..moved }, (29, 7))
            .is_err());
        assert_eq!(source.screen_rect(0), moved);
        assert!(source
            .refresh_shared_preview(moved, (29, 7))
            .unwrap()
            .is_none());
        // 读取仍覆盖 17017 行，而不是把 1024 行显示预览当作底图。
        assert_eq!(
            source.read_region(source.full()).unwrap().bgra,
            raw.as_bytes(0).unwrap()
        );
    }

    #[test]
    fn projected_shared_budget_vertical_horizontal_and_noop_key() {
        for (width, height) in [(33, 32_000), (32_000, 33)] {
            let raw = render_image(
                width,
                height,
                [13, 27, 199, 255].repeat((width * height) as usize),
            );
            let (mut source, previews) =
                CanvasSource::shared_image(raw.clone(), (width, height)).unwrap();
            let full = source.full();
            let initial = if width < height { (2, 1024) } else { (1024, 2) };
            let actual = previews[0].size(0);
            assert_eq!((u32::from(actual.width), u32::from(actual.height)), initial);
            assert!(source
                .refresh_shared_preview(full, (1024, 1024))
                .unwrap()
                .is_none());
            let preview = source
                .refresh_shared_preview(full, (400, 700))
                .unwrap()
                .unwrap();
            let actual = preview.size(0);
            let size = (u32::from(actual.width), u32::from(actual.height));
            assert_eq!(size, if width < height { (1, 700) } else { (400, 1) });
            assert!(source
                .refresh_shared_preview(full, (400, 700))
                .unwrap()
                .is_none());
            assert!(source
                .refresh_shared_preview(full, (0, 700))
                .unwrap()
                .is_none());
            let capped = source
                .refresh_shared_preview(full, (u32::MAX, u32::MAX))
                .unwrap()
                .unwrap();
            let actual = capped.size(0);
            assert!(u32::from(actual.width).max(u32::from(actual.height)) <= PROJECTED_MAX_SIDE);
            assert!(capped.as_bytes(0).unwrap().len() as u64 <= PROJECTED_MAX_PIXELS * 4);
            assert!(std::ptr::eq(source.original(&capped), raw.as_ref()));
            assert!(source
                .refresh_shared_preview(full, (u32::MAX, u32::MAX))
                .unwrap()
                .is_none());
        }
        // 用尺寸 oracle 检查 30.72MP 长图预算，不为纯限制验证分配 117MiB 原图。
        let vertical = projected_size((960, 32_000), (u32::MAX, u32::MAX)).unwrap();
        let horizontal = projected_size((32_000, 960), (u32::MAX, u32::MAX)).unwrap();
        assert_eq!(vertical, (489, 16_329));
        assert_eq!(horizontal, (16_329, 489));
        assert!(u64::from(vertical.0) * u64::from(vertical.1) <= PROJECTED_MAX_PIXELS);
        assert_eq!(projected_size((960, 32_000), (960, 500)), Some((15, 500)));
        let capped_vertical = projection_grid(Rect::full(960, 32_000), 1.).unwrap();
        let capped_horizontal = projection_grid(Rect::full(32_000, 960), 1.).unwrap();
        assert_eq!(capped_vertical.density, capped_horizontal.density);
        assert_eq!(
            capped_vertical.pixel_bounds.width,
            capped_horizontal.pixel_bounds.height
        );
        assert_eq!(
            capped_vertical.pixel_bounds.height,
            capped_horizontal.pixel_bounds.width
        );
        assert!(
            u64::from(capped_vertical.pixel_bounds.width)
                * u64::from(capped_vertical.pixel_bounds.height)
                <= PROJECTED_MAX_PIXELS
        );
        assert!(
            capped_vertical
                .pixel_bounds
                .width
                .max(capped_vertical.pixel_bounds.height)
                <= PROJECTED_MAX_SIDE
        );
    }

    #[test]
    fn projected_shared_box_averages_fractional_color_blocks_and_original_checker() {
        // 6→4 的目标格宽为 1.5 源像素，不能退化为取最近像素或整数分桶。
        let blocks = [0, 120, 0, 120, 0, 120]
            .into_iter()
            .flat_map(|v| [v, v, v, 255])
            .collect::<Vec<u8>>();
        assert_eq!(
            projected_pixels((6, 1), &blocks, Rect::full(6, 1), (4, 1)).unwrap(),
            [40, 40, 80, 80]
                .into_iter()
                .flat_map(|v| [v, v, v, 255])
                .collect::<Vec<u8>>()
        );
        // 原坐标 16px 棋盘有等面积的 224/192，两种背景平均应为 208。
        let transparent = [9, 19, 29, 0].repeat(32 * 32);
        assert_eq!(
            projected_pixels((32, 32), &transparent, Rect::full(32, 32), (1, 1)).unwrap(),
            [208, 208, 208, 255]
        );
        let partial = [20, 40, 80, 128].repeat(32 * 32);
        assert_eq!(
            projected_pixels((32, 32), &partial, Rect::full(32, 32), (1, 1)).unwrap(),
            [114, 124, 144, 255]
        );
        // 相同比例 1:1 也只对显示结果混合，输入 alpha 和颜色不能被改写。
        assert_eq!(
            projected_pixels((32, 32), &partial, Rect::full(32, 32), (32, 32)).unwrap(),
            checker_pixels(32, &partial, (0, 0))
        );
        let nonzero_checker = Rect {
            x: 16,
            y: 0,
            width: 16,
            height: 16,
        };
        assert_eq!(
            projected_pixels((32, 32), &transparent, nonzero_checker, (1, 1)).unwrap(),
            [192, 192, 192, 255]
        );
        assert!(partial.chunks_exact(4).all(|p| p == [20, 40, 80, 128]));
        let white = [255, 255, 255, 255].repeat(33 * 65_535);
        assert_eq!(
            projected_pixels((33, 65_535), &white, Rect::full(33, 65_535), (1, 1)).unwrap(),
            [255, 255, 255, 255]
        );
        let mut edge = [0, 0, 0, 255].repeat(8);
        edge[3] = 0;
        let grid = projection_grid(
            Rect {
                x: 1,
                y: 0,
                width: 1,
                height: 1,
            },
            0.375,
        )
        .unwrap();
        // 请求区域本身不透明，但全图采样格还包含 ROI 外的透明像素。
        assert_eq!(
            projected_grid_pixels((8, 1), &edge, grid).unwrap(),
            [84, 84, 84, 255]
        );
    }

    #[test]
    fn projected_shared_global_grid_keeps_fractional_overlap_pixels_and_checker_phase() {
        let (width, height) = (96, 17_000);
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                bytes.extend_from_slice(&[
                    if x % 2 == 0 { 5 } else { 250 },
                    if y % 2 == 0 { 17 } else { 231 },
                    ((x * 23 + y * 7) % 251) as u8,
                    [0, 128, 255][((x + y) % 3) as usize],
                ]);
            }
        }
        let raw = render_image(width, height, bytes);
        let (mut source, _) = CanvasSource::shared_image(raw.clone(), (width, height)).unwrap();
        let a = Rect {
            x: 7,
            y: 4096,
            width: 69,
            height: 48,
        };
        let b = Rect {
            x: 8,
            y: 4098,
            width: 69,
            height: 48,
        };
        for density in [0.375_f32, 0.37] {
            let first = source
                .refresh_shared_preview_at(a, density)
                .unwrap()
                .unwrap();
            let Backend::ProjectedImage {
                grid: first_grid, ..
            } = &source.backend
            else {
                unreachable!()
            };
            let first_grid = *first_grid;
            assert_eq!(first_grid.density, f64::from(density));
            let bounds = source.preview_bounds(0);
            assert_eq!(
                bounds.x,
                (f64::from(first_grid.pixel_bounds.x) / f64::from(density)) as f32
            );
            assert_eq!(
                bounds.y,
                (f64::from(first_grid.pixel_bounds.y) / f64::from(density)) as f32
            );
            assert_eq!(
                bounds.width,
                (f64::from(first_grid.pixel_bounds.width) / f64::from(density)) as f32
            );
            if density == 0.37 {
                let same_grid = Rect { x: 8, y: 4097, ..a };
                assert!(source
                    .refresh_shared_preview_at(same_grid, density)
                    .unwrap()
                    .is_none());
                assert_eq!(source.preview_bounds(0), bounds);
            }
            let second = source
                .refresh_shared_preview_at(b, density)
                .unwrap()
                .unwrap();
            let Backend::ProjectedImage {
                grid: second_grid, ..
            } = &source.backend
            else {
                unreachable!()
            };
            let second_grid = *second_grid;
            let overlap = first_grid
                .pixel_bounds
                .intersection(second_grid.pixel_bounds)
                .unwrap();
            assert!(overlap.width > 20 && overlap.height > 10);
            for y in overlap.y..overlap.y + overlap.height {
                for x in overlap.x..overlap.x + overlap.width {
                    let i = (((y - first_grid.pixel_bounds.y) * first_grid.pixel_bounds.width + x
                        - first_grid.pixel_bounds.x)
                        * 4) as usize;
                    let j = (((y - second_grid.pixel_bounds.y) * second_grid.pixel_bounds.width
                        + x
                        - second_grid.pixel_bounds.x)
                        * 4) as usize;
                    assert_eq!(
                        &first.as_bytes(0).unwrap()[i..i + 4],
                        &second.as_bytes(0).unwrap()[j..j + 4]
                    );
                }
            }
            assert_eq!(source.original_rect(0), Rect::full(width, height));
            assert!(std::ptr::eq(source.original(&second), raw.as_ref()));
            assert!(source
                .refresh_shared_preview_at(b, density)
                .unwrap()
                .is_none());
            assert!(source.refresh_shared_preview_at(b, f32::NAN).is_err());
            assert!(source.refresh_shared_preview_at(b, 0.).is_err());
            assert!(source
                .refresh_shared_preview_at(b, density)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn projected_shared_rejects_malformed_source_and_keeps_small_image_contract() {
        let raw = render_image(64, 120, [13, 27, 199, 255].repeat(64 * 120));
        let (mut source, previews) = CanvasSource::shared_image(raw.clone(), (64, 120)).unwrap();
        assert!(matches!(source.backend, Backend::Image(_)));
        assert!(Arc::ptr_eq(&raw, &previews[0]));
        assert!(source
            .refresh_shared_preview(Rect::full(64, 120), (4, 4))
            .unwrap()
            .is_none());
        assert_eq!(source.original_rect(0), source.screen_rect(0));
        assert!(CanvasSource::shared_image(raw.clone(), (64, 32_000)).is_err());
        assert!(CanvasSource::shared_image(raw, (0, 120)).is_err());
        assert!(projected_pixels((6, 1), &[0; 3], Rect::full(6, 1), (4, 1)).is_err());
        assert!(projected_pixels((6, 1), &[0; 24], Rect::full(6, 1), (7, 1)).is_err());
        assert!(projected_pixels((6, 1), &[0; 24], Rect::full(6, 1), (0, 1)).is_err());
    }

    #[test]
    fn transparent_file_crops_original_alpha_and_restores_aligned_checker_preview() {
        let bytes = [20, 40, 80, 0, 20, 40, 80, 128, 20, 40, 80, 255].repeat(20);
        let image = Image {
            width: 30,
            height: 2,
            bgra: bytes.clone(),
            readback_ms: 0.,
        };
        let (source, preview) = CanvasSource::image(image).unwrap();
        assert_eq!(source.read_region(source.full()).unwrap().bgra, bytes);
        assert_eq!(
            &preview[0].as_bytes(0).unwrap()[0..4],
            &[224, 224, 224, 255]
        );
        let rect = Rect {
            x: 15,
            y: 0,
            width: 6,
            height: 2,
        };
        let cropped = source.read_region(rect).unwrap();
        let restore = source.restore_preview(&cropped, rect);
        for y in 0..2_usize {
            assert_eq!(
                &restore[y * 6 * 4..(y + 1) * 6 * 4],
                &preview[0].as_bytes(0).unwrap()[(y * 30 + 15) * 4..(y * 30 + 21) * 4]
            );
        }
        assert!(source
            .read_region(Rect {
                x: 29,
                y: 0,
                width: 2,
                height: 1
            })
            .is_err());
    }
    #[test]
    fn opaque_file_shares_preview_storage_and_rejects_malformed_input() {
        let image = Image {
            width: 4,
            height: 3,
            bgra: [3, 5, 7, 255].repeat(12),
            readback_ms: 0.,
        };
        let (source, preview) = CanvasSource::image(image).unwrap();
        assert!(std::ptr::eq(
            source.original(&preview[0]),
            preview[0].as_ref()
        ));
        assert!(CanvasSource::image(Image {
            width: 2,
            height: 2,
            bgra: vec![0; 3],
            readback_ms: 0.
        })
        .is_err());
    }

    #[test]
    fn long_image_tiles_preserve_alpha_cross_tile_crops_and_global_checker() {
        for (width, height) in [(33, 17017), (17017, 35)] {
            let mut bytes = Vec::with_capacity(width as usize * height as usize * 4);
            for y in 0..height {
                for x in 0..width {
                    bytes.extend_from_slice(&[
                        ((x * 17 + y * 13) % 251) as u8,
                        ((x * 7 + y * 23) % 251) as u8,
                        ((x * 19 + y * 3) % 251) as u8,
                        [0, 128, 255][((x + y) % 3) as usize],
                    ]);
                }
            }
            let (source, previews) = CanvasSource::image(Image {
                width,
                height,
                bgra: bytes.clone(),
                readback_ms: 0.,
            })
            .unwrap();
            assert!(source.is_image());
            assert_eq!(source.full(), Rect::full(width, height));
            assert_eq!(source.screen_count(), previews.len());
            assert_eq!(source.current, 0);
            assert_eq!(source.monitor_at((width as i32 - 1, height as i32 - 1)), 0);
            assert_eq!(source.read_region(source.full()).unwrap().bgra, bytes);
            for (i, preview) in previews.iter().enumerate() {
                let rect = source.screen_rect(i);
                assert!(rect.width <= IMAGE_TILE_SIDE && rect.height <= IMAGE_TILE_SIDE);
                let original = source.original(preview).as_bytes(0).unwrap();
                assert_eq!(
                    original.len(),
                    rect.width as usize * rect.height as usize * 4
                );
                for (x, y) in [(0, 0), (rect.width - 1, rect.height - 1)] {
                    let global = ((rect.y + y) * width + rect.x + x) as usize * 4;
                    let local = (y * rect.width + x) as usize * 4;
                    assert_eq!(&original[local..local + 4], &bytes[global..global + 4]);
                    let p = &bytes[global..global + 4];
                    let gray = if ((rect.x + x) / 16 + (rect.y + y) / 16) % 2 == 0 {
                        224
                    } else {
                        192
                    };
                    let expected: Vec<_> = p[..3]
                        .iter()
                        .map(|&c| {
                            ((c as u32 * p[3] as u32 + gray * (255 - p[3] as u32) + 127) / 255)
                                as u8
                        })
                        .chain([255])
                        .collect();
                    assert_eq!(&preview.as_bytes(0).unwrap()[local..local + 4], expected);
                }
            }
            let rect = if width > 16384 {
                Rect {
                    x: 4093,
                    y: 2,
                    width: 7,
                    height: 29,
                }
            } else {
                Rect {
                    x: 2,
                    y: 4093,
                    width: 29,
                    height: 7,
                }
            };
            let cropped = source.read_region(rect).unwrap();
            for y in 0..rect.height {
                let source_row = ((rect.y + y) * width + rect.x) as usize * 4;
                let output_row = (y * rect.width) as usize * 4;
                assert_eq!(
                    &cropped.bgra[output_row..output_row + rect.width as usize * 4],
                    &bytes[source_row..source_row + rect.width as usize * 4]
                );
            }
        }
    }

    #[test]
    fn long_image_opaque_tiles_share_preview_and_enforce_cpu_limits() {
        let (source, previews) = CanvasSource::image(Image {
            width: 32,
            height: 65535,
            bgra: [13, 27, 199, 255].repeat(32 * 65535),
            readback_ms: 0.,
        })
        .unwrap();
        let Backend::ImageTiles(tiles) = &source.backend else {
            panic!("long image must use tiles");
        };
        assert_eq!(
            tiles
                .iter()
                .map(|t| t.original.as_bytes(0).unwrap().len())
                .sum::<usize>(),
            32 * 65535 * 4
        );
        assert!(tiles
            .iter()
            .zip(&previews)
            .all(|(t, p)| Arc::ptr_eq(&t.original, p)));
        for (width, height) in [(32, 65536), (65536, 32), (640, 50001)] {
            assert!(CanvasSource::image(Image {
                width,
                height,
                bgra: Vec::new(),
                readback_ms: 0.
            })
            .is_err());
        }
        let original = render_image(64, 120, [13, 27, 199, 255].repeat(64 * 120));
        let (shared, small_previews) =
            CanvasSource::shared_image(original.clone(), (64, 120)).unwrap();
        assert!(Arc::ptr_eq(&original, &small_previews[0]));
        assert!(std::ptr::eq(
            shared.original(&small_previews[0]),
            original.as_ref()
        ));
    }

    #[test]
    fn solid_canvas_retains_only_fill_and_reads_exact_edge_roi_bgra() {
        let fill = [3, 5, 7, 128];
        let source = CanvasSource::solid(1000, 32000, fill).unwrap();
        // Backend 只持有四个通道，不创建 Image / RenderImage 或预览纹理。
        assert!(matches!(&source.backend, Backend::Solid(value) if *value == fill));
        assert_eq!(source.solid_fill(), Some(fill));
        assert_eq!(source.full(), Rect::full(1000, 32000));
        assert!(source.is_image());
        assert_eq!(source.screen_count(), 1);
        let roi = Rect {
            x: 997,
            y: 31998,
            width: 3,
            height: 2,
        };
        let image = source.read_region(roi).unwrap();
        assert_eq!((image.width, image.height), (3, 2));
        assert_eq!(image.bgra, fill.repeat(6));
        assert_eq!(image.readback_ms, 0.);
        assert!(source.read_region(Rect { width: 4, ..roi }).is_err());
    }

    #[test]
    fn solid_canvas_rejects_invalid_sizes_and_keeps_exact_capacity_boundaries() {
        for (width, height) in [(0, 1), (1, 0), (65536, 1), (1, 65536), (640, 50001)] {
            assert!(CanvasSource::solid(width, height, [0, 0, 0, 255]).is_err());
        }
        for (width, height) in [(1, 1), (65535, 1), (1, 65535), (640, 50000)] {
            let source = CanvasSource::solid(width, height, [0, 0, 0, 255]).unwrap();
            assert_eq!(source.full(), Rect::full(width, height));
            assert!(matches!(source.backend, Backend::Solid(_)));
        }
    }

    #[test]
    fn solid_canvas_fill_change_invalidates_scene_mosaic_cache_and_sampling() {
        use crate::{
            annotation::{Annotation, Point, Style, Tool},
            scene::{Layer, Scene, SourceIdentity},
        };
        let before_fill = [3, 5, 7, 255];
        let after_fill = [17, 31, 47, 255];
        let mut source = CanvasSource::solid(16, 12, before_fill).unwrap();
        let before_pixels = SourcePixels {
            canvas: source.full(),
            fill: source.solid_fill(),
            planes: Vec::new(),
            overlay: None,
        };
        let mut mosaic = Annotation::new(
            Tool::Mosaic,
            Point { x: 2., y: 2. },
            Style {
                width: 4.,
                ..Default::default()
            },
        );
        mosaic.update(Point { x: 14., y: 10. });
        let annotations = [mosaic];
        let before = Scene::default().updated(&annotations, &before_pixels);
        assert!(before.errors.is_empty());
        let Layer::Mosaic { blocks: old } = &before.layers[0] else {
            panic!("missing mosaic");
        };
        assert!(!old.is_empty());
        assert!(old.iter().all(|block| block.color == 0x070503));
        assert!(!source.set_solid_fill(before_fill));
        let unchanged = before.updated(&annotations, &before_pixels);
        let Layer::Mosaic { blocks: reused } = &unchanged.layers[0] else {
            panic!("missing mosaic");
        };
        assert!(Arc::ptr_eq(old, reused));

        assert!(source.set_solid_fill(after_fill));
        let after_pixels = SourcePixels {
            canvas: source.full(),
            fill: source.solid_fill(),
            planes: Vec::new(),
            overlay: None,
        };
        assert!(SourceIdentity::of(&before_pixels) != SourceIdentity::of(&after_pixels));
        let after = unchanged.updated(&annotations, &after_pixels);
        assert!(after.errors.is_empty());
        let Layer::Mosaic { blocks: refreshed } = &after.layers[0] else {
            panic!("missing mosaic");
        };
        assert!(!Arc::ptr_eq(reused, refreshed));
        assert!(refreshed.iter().all(|block| block.color == 0x2f1f11));
        let mut output = source.read_region(source.full()).unwrap();
        after.apply(&mut output, source.full());
        assert_eq!(output.bgra, after_fill.repeat(16 * 12));
    }

    #[test]
    fn solid_source_has_no_preview_and_fills_only_requested_output() {
        let fill = [1, 2, 3, 255];
        let mut source = CanvasSource::solid(700, 16000, fill).unwrap();
        assert!(source.is_image());
        assert_eq!(source.screen_count(), 1);
        assert_eq!(source.solid_fill(), Some(fill));
        let region = Rect {
            x: 10,
            y: 20,
            width: 3,
            height: 2,
        };
        assert_eq!(source.read_region(region).unwrap().bgra, fill.repeat(6));
        assert!(!source.set_solid_fill(fill));
        assert!(source.set_solid_fill([9, 8, 7, 255]));
        assert_eq!(
            source.read_region(region).unwrap().bgra,
            [9, 8, 7, 255].repeat(6)
        );
        assert!(CanvasSource::solid(0, 1, fill).is_err());
        assert!(CanvasSource::solid(65_535, 65_535, fill).is_err());
    }

    #[test]
    fn presentation_factory_preserves_raw_alpha_and_bounds_long_target_tiles_without_box() {
        use crate::pin_presentation::{CornerCache, PhysicalPoint, PresentationPreviewPlan};
        use crate::selection_finish::SelectionFinishStyle;

        for width in [31_u32, 16385] {
            let height = 3;
            let bytes: Vec<_> = (0..width * height)
                .flat_map(|i| {
                    [
                        (i * 7) as u8,
                        (i * 13) as u8,
                        (i * 19) as u8,
                        [0, 97, 128, 255][i as usize % 4],
                    ]
                })
                .collect();
            let raw = render_image(width, height, bytes.clone());
            let (mut source, previews) =
                CanvasSource::shared_presentation_image(raw.clone(), (width, height)).unwrap();
            assert!(source.is_image());
            assert_eq!(source.is_projected_image(), width > 16384);
            assert_eq!(source.full(), Rect::full(width, height));
            assert_eq!(source.original_rect(0), source.full());
            assert!(Arc::ptr_eq(&source.shared_original().unwrap(), &raw));
            assert_eq!(source.original(&previews[0]).id, raw.id);
            assert_eq!(source.original(&previews[0]).as_bytes(0).unwrap(), bytes);
            let read = source.read_region(source.full()).unwrap();
            assert_eq!(read.bgra, bytes);
            assert_eq!(source.restore_preview(&read, source.full()), bytes);
            assert!(source
                .refresh_shared_preview_at(source.full(), 0.3125)
                .unwrap()
                .is_none());
            if width <= 16384 {
                assert!(Arc::ptr_eq(&previews[0], &raw));
            } else {
                let preview_size = previews[0].size(0);
                assert_eq!(
                    (
                        u32::from(preview_size.width),
                        u32::from(preview_size.height)
                    ),
                    (1, 1)
                );
                assert_eq!(previews[0].as_bytes(0).unwrap(), [0; 4]);
                let pixels = SourcePixels {
                    canvas: source.full(),
                    fill: None,
                    planes: vec![PixelPlane {
                        rect: source.full(),
                        bgra: raw.as_bytes(0).unwrap(),
                    }],
                    overlay: None,
                };
                let plan = PresentationPreviewPlan::new(
                    source.full(),
                    SelectionFinishStyle::default(),
                    PhysicalPoint { x: -3.25, y: 5.75 },
                    0.3125,
                )
                .unwrap();
                let mut cache = CornerCache::default();
                cache
                    .prepare_body(plan, raw.id.0, &pixels, source.full())
                    .unwrap();
                assert_eq!(cache.body_tiles().len(), 2);
                let mut covered = 0_u64;
                let mut previous_end = None;
                for tile in cache.body_tiles() {
                    assert!(tile.region.width <= 4096 && tile.region.height <= 4096);
                    assert_eq!(tile.region.top, 5);
                    assert_eq!(tile.region.height, 2);
                    if let Some(end) = previous_end {
                        assert_eq!(tile.region.left, end);
                    }
                    previous_end = Some(tile.region.left + tile.region.width as i32);
                    covered += u64::from(tile.region.width) * u64::from(tile.region.height);
                    let expected = plan
                        .render_content_region(
                            &pixels,
                            &crate::scene::Scene::default(),
                            None,
                            tile.region,
                        )
                        .unwrap();
                    assert_eq!(tile.image.as_bytes(0).unwrap(), expected.bgra);
                }
                assert_eq!(covered, 5122 * 2);
                assert_eq!(previous_end, Some(5118));
                assert_eq!(raw.as_bytes(0).unwrap(), bytes);
            }
        }
    }
}
