//! 截图贴图的内容像素与外观分离模型；所有尺寸、坐标均为物理像素。
//!
//! 来源模型保留边框内外侧；目标输出和预览小角直接在目标物理栅格绘制。
//! 不改截图普通输出，不读取 GPU，不把纯色/文字文档转为位图。
#[cfg(test)]
use crate::selection_finish::FinishLayout;
use crate::{
    capture::Image,
    crop_mask::Polygon,
    geometry::{Rect, ScreenRect},
    output_projection::OutputProjection,
    scene::{Scene, SourcePixels},
    selection_finish::SelectionFinishStyle,
    Result,
};

#[path = "pin_presentation_paint.rs"]
mod paint;
pub(crate) use paint::{paint_border, paint_shadow};
#[path = "pin_presentation_preview.rs"]
mod preview;
pub(crate) use preview::{
    fit_preview_placement_with_sizing, paint_content_regions, preview_visible_source_region,
    validate_preview_geometry_with_sizing, CornerCache,
};

const PREVIEW_TILE_SIDE: u32 = 4096;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PinPresentation {
    source_style: SelectionFinishStyle,
    #[cfg(test)]
    layout: FinishLayout,
    #[cfg(test)]
    content_screen: ScreenRect,
    outer_screen: ScreenRect,
}

impl PinPresentation {
    pub(crate) fn new(
        bounds: ScreenRect,
        selected: Rect,
        source_style: SelectionFinishStyle,
    ) -> Result<Self> {
        if !selected.valid(bounds.width, bounds.height) {
            return Err("截图贴图选区为空或超出来源".into());
        }
        checked_screen(bounds)?;
        let source_style = source_style.normalized();
        let layout = source_style.layout(selected.width, selected.height)?;
        // 与普通用户输出共享 65535 单边/3200 万像素预算，外扩部分也计入。
        OutputProjection::new(Rect::full(layout.output_width, layout.output_height), 1.)?;
        let content_screen = screen_rect(
            i64::from(bounds.left) + i64::from(selected.x),
            i64::from(bounds.top) + i64::from(selected.y),
            selected.width,
            selected.height,
        )?;
        let outer_screen = screen_rect(
            i64::from(content_screen.left) - i64::from(layout.padding.left),
            i64::from(content_screen.top) - i64::from(layout.padding.top),
            layout.output_width,
            layout.output_height,
        )?;
        Ok(Self {
            source_style,
            #[cfg(test)]
            layout,
            #[cfg(test)]
            content_screen,
            outer_screen,
        })
    }

    #[cfg(test)]
    pub(crate) fn source_style(self) -> SelectionFinishStyle {
        self.source_style
    }
    pub(crate) fn effective_style(self) -> SelectionFinishStyle {
        self.source_style
    }
    #[cfg(test)]
    pub(crate) fn layout(self) -> FinishLayout {
        self.layout
    }
    #[cfg(test)]
    pub(crate) fn content_screen(self) -> ScreenRect {
        self.content_screen
    }
    pub(crate) fn outer_screen(self) -> ScreenRect {
        self.outer_screen
    }

    /// 装饰缓存与内容像素无关；圆角遮罩使用 layout.geometry，不能填实透明角。
    #[cfg(test)]
    pub(crate) fn decoration_key(self) -> DecorationKey {
        DecorationKey {
            width: self.layout.geometry.width,
            height: self.layout.geometry.height,
            style: SelectionFinishStyle {
                remember_enabled: false,
                ..self.effective_style()
            },
        }
    }

    /// 两个独立图层：阴影在内容下，边框在内容上。仅返回几何分片，不分配像素。
    #[cfg(test)]
    pub(crate) fn decoration_parts(self) -> Vec<DecorationPart> {
        let style = self.effective_style();
        let shadow = style.shadow_enabled
            && style.shadow_blur > 0.
            && style.shadow_opacity > 0.
            && style.shadow_color >> 24 != 0;
        let border = style.border_enabled && style.border_color >> 24 != 0;
        if !shadow && !border {
            return Vec::new();
        }
        let band = self.layout.padding.left.max(self.layout.padding.top)
            + self.layout.geometry.radius.ceil() as u32
            + if border {
                style.border_thickness.ceil() as u32
            } else {
                0
            }
            + 2;
        let regions = edge_regions(self.layout.output_width, self.layout.output_height, band);
        let mut parts = Vec::new();
        for (enabled, layer) in [
            (shadow, DecorationLayer::Shadow),
            (border, DecorationLayer::Border),
        ] {
            if enabled {
                parts.extend(
                    regions
                        .iter()
                        .map(|&region| DecorationPart { region, layer }),
                );
            }
        }
        parts
    }

    /// 只生成来源尺寸的装饰小边带，不能将 raw 内容或主题颜色压入该缓存。
    #[cfg(test)]
    pub(crate) fn render_decoration(self, part: DecorationPart) -> Result<Image> {
        let (width, height) = (self.layout.geometry.width, self.layout.geometry.height);
        if part.region.width > PREVIEW_TILE_SIDE || part.region.height > PREVIEW_TILE_SIDE {
            return Err("贴图装饰分片超过 4096 物理像素".into());
        }
        match part.layer {
            DecorationLayer::Shadow => {
                self.effective_style()
                    .shadow_region(width, height, part.region)
            }
            DecorationLayer::Border => {
                self.effective_style()
                    .border_region(width, height, part.region)
            }
        }
    }

    /// 像素中心映回来源外观坐标后，共用既有圆角覆盖率；不修改 raw alpha。
    #[cfg(test)]
    pub(crate) fn content_coverage(self, outer_point: PhysicalPoint) -> f64 {
        self.layout.geometry.coverage_at(
            outer_point.x - f64::from(self.layout.content_rect.x),
            outer_point.y - f64::from(self.layout.content_rect.y),
        )
    }

    /// 预览必须使用已确认的整个窗口物理尺寸；不能给各层单独舍入宽高/边距。
    #[cfg(test)]
    pub(crate) fn preview_projection(
        self,
        physical_size: (u32, u32),
        orientation: PinOrientation,
    ) -> Result<PresentationProjection> {
        PresentationProjection::new(self.layout, physical_size, orientation)
    }

    /// 仅是导出坐标/预算计划；本阶段不提供放大/旋转的像素渲染接口。
    #[cfg(test)]
    pub(crate) fn output_projection(
        self,
        zoom: f32,
        orientation: PinOrientation,
    ) -> Result<PresentationProjection> {
        let (width, height) =
            orientation.dimensions(self.layout.output_width, self.layout.output_height);
        let output = OutputProjection::new(Rect::full(width, height), zoom)?;
        PresentationProjection::new(self.layout, (output.width, output.height), orientation)
    }
}

/// 只用于已裁剪、按用户选择压入标注的截图。文字/纯色文档不走此入口。
pub(crate) struct ScreenshotPinPayload {
    raw: Image,
    presentation: PinPresentation,
}
impl ScreenshotPinPayload {
    pub(crate) fn new(
        bounds: ScreenRect,
        selected: Rect,
        raw: Image,
        source_style: SelectionFinishStyle,
    ) -> Result<Self> {
        let presentation = PinPresentation::new(bounds, selected, source_style)?;
        let expected = (raw.width as usize)
            .checked_mul(raw.height as usize)
            .and_then(|value| value.checked_mul(4))
            .ok_or("截图贴图像素尺寸溢出")?;
        if raw.width != selected.width
            || raw.height != selected.height
            || raw.bgra.len() != expected
        {
            return Err("截图贴图 raw 像素与选区尺寸不一致".into());
        }
        Ok(Self { raw, presentation })
    }
    pub(crate) fn into_parts(self) -> (Image, PinPresentation) {
        (self.raw, self.presentation)
    }
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &Image {
        &self.raw
    }
    pub(crate) fn presentation(&self) -> PinPresentation {
        self.presentation
    }

    /// 输出时才复制/合成来源尺寸的 raw；原 raw/alpha 永不被改变。
    /// 这里只支持 identity 外观合成：不旋转、不翻转、不放大。
    /// 不含后续 Pin 标注、处理、窗口透明度，不能直接替代宿主最终导出。
    #[cfg(test)]
    pub(crate) fn render_source_size(&self) -> Result<Image> {
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(self.raw.bgra.len())?;
        bgra.extend_from_slice(&self.raw.bgra);
        self.presentation.effective_style().compose(Image {
            width: self.raw.width,
            height: self.raw.height,
            bgra,
            readback_ms: self.raw.readback_ms,
        })
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DecorationKey {
    width: u32,
    height: u32,
    style: SelectionFinishStyle,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecorationLayer {
    Shadow,
    Border,
}
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DecorationPart {
    pub(crate) region: Rect,
    pub(crate) layer: DecorationLayer,
}

/// 使用连续边缘坐标，绝不把像素索引的 width-1 用于几何矩形边缘。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PhysicalPoint {
    pub(crate) x: f64,
    pub(crate) y: f64,
}
impl PhysicalPoint {
    /// 唯一的 DPI 换算边界；模型和导出均保留物理坐标。
    pub(crate) fn to_dip(self, pixels_per_dip: f64) -> Option<Self> {
        if !(self.x.is_finite()
            && self.y.is_finite()
            && pixels_per_dip.is_finite()
            && pixels_per_dip > 0.)
        {
            return None;
        }
        let dip = Self {
            x: self.x / pixels_per_dip,
            y: self.y / pixels_per_dip,
        };
        (dip.x.is_finite() && dip.y.is_finite()).then_some(dip)
    }
}

/// 与 Adjustments 相同：顺时针四分之一旋转后，在可见方向执行翻转。
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PinOrientation {
    turns: u8,
    flip_x: bool,
    flip_y: bool,
}
#[cfg(test)]
impl PinOrientation {
    pub(crate) fn new(turns: u8, flip_x: bool, flip_y: bool) -> Self {
        Self {
            turns: turns % 4,
            flip_x,
            flip_y,
        }
    }
    fn dimensions(self, width: u32, height: u32) -> (u32, u32) {
        if self.turns.is_multiple_of(2) {
            (width, height)
        } else {
            (height, width)
        }
    }
    fn map(self, p: PhysicalPoint, width: u32, height: u32) -> PhysicalPoint {
        let (w, h) = (f64::from(width), f64::from(height));
        let (mut x, mut y) = match self.turns {
            1 => (h - p.y, p.x),
            2 => (w - p.x, h - p.y),
            3 => (p.y, w - p.x),
            _ => (p.x, p.y),
        };
        let (rw, rh) = self.dimensions(width, height);
        if self.flip_x {
            x = f64::from(rw) - x;
        }
        if self.flip_y {
            y = f64::from(rh) - y;
        }
        PhysicalPoint { x, y }
    }
    fn unmap(self, p: PhysicalPoint, width: u32, height: u32) -> PhysicalPoint {
        let (rw, rh) = self.dimensions(width, height);
        let x = if self.flip_x {
            f64::from(rw) - p.x
        } else {
            p.x
        };
        let y = if self.flip_y {
            f64::from(rh) - p.y
        } else {
            p.y
        };
        let (w, h) = (f64::from(width), f64::from(height));
        let (x, y) = match self.turns {
            1 => (y, h - x),
            2 => (w - x, h - y),
            3 => (w - y, x),
            _ => (x, y),
        };
        PhysicalPoint { x, y }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PresentationProjection {
    layout: FinishLayout,
    orientation: PinOrientation,
    size: (u32, u32),
    sx: f64,
    sy: f64,
}
#[cfg(test)]
impl PresentationProjection {
    fn new(layout: FinishLayout, size: (u32, u32), orientation: PinOrientation) -> Result<Self> {
        if size.0 == 0 || size.1 == 0 {
            return Err("贴图外观投影的物理尺寸为空".into());
        }
        let (width, height) = orientation.dimensions(layout.output_width, layout.output_height);
        Ok(Self {
            layout,
            orientation,
            size,
            sx: f64::from(size.0) / f64::from(width),
            sy: f64::from(size.1) / f64::from(height),
        })
    }
    pub(crate) fn physical_size(self) -> (u32, u32) {
        self.size
    }
    pub(crate) fn map_outer(self, p: PhysicalPoint) -> PhysicalPoint {
        let p = self
            .orientation
            .map(p, self.layout.output_width, self.layout.output_height);
        PhysicalPoint {
            x: p.x * self.sx,
            y: p.y * self.sy,
        }
    }
    pub(crate) fn unmap_outer(self, p: PhysicalPoint) -> PhysicalPoint {
        self.orientation.unmap(
            PhysicalPoint {
                x: p.x / self.sx,
                y: p.y / self.sy,
            },
            self.layout.output_width,
            self.layout.output_height,
        )
    }
    pub(crate) fn map_content(self, p: PhysicalPoint) -> PhysicalPoint {
        self.map_outer(PhysicalPoint {
            x: p.x + f64::from(self.layout.content_rect.x),
            y: p.y + f64::from(self.layout.content_rect.y),
        })
    }
    pub(crate) fn unmap_content(self, p: PhysicalPoint) -> PhysicalPoint {
        let p = self.unmap_outer(p);
        PhysicalPoint {
            x: p.x - f64::from(self.layout.content_rect.x),
            y: p.y - f64::from(self.layout.content_rect.y),
        }
    }
    pub(crate) fn content_quad(self) -> [PhysicalPoint; 4] {
        let (w, h) = (
            f64::from(self.layout.geometry.width),
            f64::from(self.layout.geometry.height),
        );
        [
            PhysicalPoint { x: 0., y: 0. },
            PhysicalPoint { x: w, y: 0. },
            PhysicalPoint { x: w, y: h },
            PhysicalPoint { x: 0., y: h },
        ]
        .map(|p| self.map_content(p))
    }
}

fn checked_screen(rect: ScreenRect) -> Result<ScreenRect> {
    screen_rect(
        i64::from(rect.left),
        i64::from(rect.top),
        rect.width,
        rect.height,
    )
}
fn screen_rect(left: i64, top: i64, width: u32, height: u32) -> Result<ScreenRect> {
    if width == 0 || height == 0 {
        return Err("截图贴图全局物理范围为空".into());
    }
    let right = left + i64::from(width);
    let bottom = top + i64::from(height);
    let left = i32::try_from(left).map_err(|_| "截图贴图左边界溢出")?;
    let top = i32::try_from(top).map_err(|_| "截图贴图上边界溢出")?;
    i32::try_from(right).map_err(|_| "截图贴图右边界溢出")?;
    i32::try_from(bottom).map_err(|_| "截图贴图下边界溢出")?;
    Ok(ScreenRect {
        left,
        top,
        width,
        height,
    })
}

#[cfg(test)]
fn edge_regions(width: u32, height: u32, band: u32) -> Vec<Rect> {
    let top = band.min(height.div_ceil(2));
    let bottom = band.min(height - top);
    let middle = height - top - bottom;
    let left = band.min(width.div_ceil(2));
    let right = band.min(width - left);
    let mut tiles = Vec::new();
    for region in [
        Rect {
            x: 0,
            y: 0,
            width,
            height: top,
        },
        Rect {
            x: 0,
            y: height - bottom,
            width,
            height: bottom,
        },
        Rect {
            x: 0,
            y: top,
            width: left,
            height: middle,
        },
        Rect {
            x: width - right,
            y: top,
            width: right,
            height: middle,
        },
    ] {
        for y in (region.y..region.y + region.height).step_by(PREVIEW_TILE_SIDE as usize) {
            for x in (region.x..region.x + region.width).step_by(PREVIEW_TILE_SIDE as usize) {
                tiles.push(Rect {
                    x,
                    y,
                    width: (region.x + region.width - x).min(PREVIEW_TILE_SIDE),
                    height: (region.y + region.height - y).min(PREVIEW_TILE_SIDE),
                });
            }
        }
    }
    tiles
}

/// 完整目标栅格中的分数矩形；直到 GPUI 最后一步才转 DIP/f32。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PresentationBounds {
    pub(crate) left: f64,
    pub(crate) top: f64,
    pub(crate) width: f64,
    pub(crate) height: f64,
}

/// 客户区物理像素栅格的局部块；left/top 可为负，不能丢弃目标相位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PresentationRegion {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TargetGeometry {
    style: SelectionFinishStyle,
    rounded: crate::selection_finish::RoundedGeometry,
    origin: PhysicalPoint,
    sx: f64,
    sy: f64,
}
impl TargetGeometry {
    fn new(
        content: Rect,
        style: SelectionFinishStyle,
        origin: PhysicalPoint,
        sx: f64,
        sy: f64,
    ) -> Self {
        let style = style.normalized();
        Self {
            style,
            rounded: style.geometry(content.width, content.height),
            origin,
            sx,
            sy,
        }
    }
    fn local(self, p: PhysicalPoint) -> PhysicalPoint {
        PhysicalPoint {
            x: (p.x - self.origin.x) / self.sx,
            y: (p.y - self.origin.y) / self.sy,
        }
    }
    fn coverage(self, p: PhysicalPoint) -> f64 {
        let source = self.local(p);
        let sd = self.rounded.signed_distance(source.x, source.y);
        let reach = (0.5 / self.sx).hypot(0.5 / self.sy);
        if sd <= -reach {
            return 1.;
        }
        if sd >= reach {
            return 0.;
        }
        let mut covered = 0;
        for y in 0..8 {
            for x in 0..8 {
                let q = self.local(PhysicalPoint {
                    x: p.x - 0.5 + (f64::from(x) + 0.5) / 8.,
                    y: p.y - 0.5 + (f64::from(y) + 0.5) / 8.,
                });
                covered += u32::from(self.rounded.contains(q.x, q.y));
            }
        }
        f64::from(covered) / 64.
    }
    fn decoration(self, p: PhysicalPoint, layer: DecorationLayer) -> [u8; 4] {
        let p = self.local(p);
        let (color, alpha) = match layer {
            DecorationLayer::Shadow => {
                let style = self.style;
                let sd = self.rounded.signed_distance(p.x, p.y);
                let strength = style.shadow_strength(sd);
                if !style.shadow_enabled || strength <= 0. {
                    return [0; 4];
                }
                (
                    style.shadow_color,
                    f64::from(style.shadow_opacity) * strength,
                )
            }
            DecorationLayer::Border => {
                if !self.style.border_enabled {
                    return [0; 4];
                }
                (
                    self.style.border_color,
                    self.rounded.border_coverage_at(
                        p.x,
                        p.y,
                        self.style.border_inside,
                        f64::from(self.style.border_thickness),
                        1. / self.sx,
                        1. / self.sy,
                    ),
                )
            }
        };
        let alpha = (f64::from((color >> 24) as u8) * alpha)
            .round()
            .clamp(0., 255.) as u8;
        if alpha == 0 {
            [0; 4]
        } else {
            [color as u8, (color >> 8) as u8, (color >> 16) as u8, alpha]
        }
    }
    fn content_bounds(self) -> PresentationBounds {
        PresentationBounds {
            left: self.origin.x,
            top: self.origin.y,
            width: f64::from(self.rounded.width) * self.sx,
            height: f64::from(self.rounded.height) * self.sy,
        }
    }
    fn contour(self, outward: f64) -> PresentationContour {
        let bounds = self.content_bounds();
        PresentationContour {
            bounds: PresentationBounds {
                left: bounds.left - outward * self.sx,
                top: bounds.top - outward * self.sy,
                width: bounds.width + 2. * outward * self.sx,
                height: bounds.height + 2. * outward * self.sy,
            },
            radius_x: (self.rounded.radius + outward) * self.sx,
            radius_y: (self.rounded.radius + outward) * self.sy,
        }
    }
    fn border_contour(self, outward: f64) -> PresentationContour {
        let mut contour = self.contour(outward);
        if self.rounded.radius == 0. {
            contour.radius_x = 0.;
            contour.radius_y = 0.;
        }
        contour
    }
}

/// 预览 path 与输出距离场使用相同来源轮廓和实际目标尺度。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PresentationContour {
    pub(crate) bounds: PresentationBounds,
    pub(crate) radius_x: f64,
    pub(crate) radius_y: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PresentationOutputPlan {
    content: Rect,
    projection: OutputProjection,
    target: TargetGeometry,
}
impl PresentationOutputPlan {
    pub(crate) fn new(content: Rect, style: SelectionFinishStyle, zoom: f32) -> Result<Self> {
        if !content.valid(u32::MAX, u32::MAX) {
            return Err("贴图外观输出范围无效".into());
        }
        let style = style.normalized();
        let layout = style.layout(content.width, content.height)?;
        let whole =
            OutputProjection::new(Rect::full(layout.output_width, layout.output_height), zoom)?;
        let sx = f64::from(whole.width) / f64::from(layout.output_width);
        let sy = f64::from(whole.height) / f64::from(layout.output_height);
        let origin = PhysicalPoint {
            x: f64::from(layout.padding.left) * sx,
            y: f64::from(layout.padding.top) * sy,
        };
        let projection = OutputProjection {
            source: content,
            width: whole.width,
            height: whole.height,
            sx: sx as f32,
            sy: sy as f32,
            origin: crate::annotation::Point {
                x: -origin.x as f32,
                y: -origin.y as f32,
            },
        };
        Ok(Self {
            content,
            projection,
            target: TargetGeometry::new(content, style, origin, sx, sy),
        })
    }
    #[cfg(test)]
    pub(crate) fn physical_size(self) -> (u32, u32) {
        (self.projection.width, self.projection.height)
    }
    pub(crate) fn scene_projection(self) -> OutputProjection {
        self.projection
    }
    #[cfg(test)]
    pub(crate) fn content_bounds(self) -> PresentationBounds {
        self.target.content_bounds()
    }

    /// Scene 与套索先在完整目标栅格渲染，外观最后乘 alpha；窗口透明度由宿主再施加一次。
    pub(crate) fn render(
        self,
        source: &SourcePixels<'_>,
        scene: &Scene,
        mask: Option<&Polygon>,
    ) -> Result<Image> {
        validate_source(source, self.content)?;
        if self.target.sx == 1. && self.target.sy == 1. {
            let mut content = source.read(self.content);
            scene.try_apply(&mut content, self.content)?;
            if let Some(mask) = mask {
                mask.apply(&mut content, self.content);
            }
            return self.target.style.compose(content);
        }
        let mut image = projected_content(source, scene, mask, self.projection)?;
        for y in 0..image.height {
            for x in 0..image.width {
                let point = PhysicalPoint {
                    x: f64::from(x) + 0.5,
                    y: f64::from(y) + 0.5,
                };
                let i = (y as usize * image.width as usize + x as usize) * 4;
                let mut content: [u8; 4] = image.bgra[i..i + 4].try_into().expect("BGRA 像素");
                content[3] = (f64::from(content[3]) * self.target.coverage(point)).round() as u8;
                if content[3] == 0 {
                    content.fill(0);
                }
                let mut pixel = self.target.decoration(point, DecorationLayer::Shadow);
                blend_pixel(&mut pixel, content);
                blend_pixel(
                    &mut pixel,
                    self.target.decoration(point, DecorationLayer::Border),
                );
                image.bgra[i..i + 4].copy_from_slice(&pixel);
            }
        }
        Ok(image)
    }
}

/// 非整数等比预览的实际客户区物理几何；该值可直接参与外部 Scene/image/DPI 缓存键。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PresentationPreviewPlan {
    content: Rect,
    #[cfg(test)]
    layout: FinishLayout,
    target: TargetGeometry,
    raster: PresentationRegion,
}
impl PresentationPreviewPlan {
    pub(crate) fn new(
        content: Rect,
        style: SelectionFinishStyle,
        content_origin: PhysicalPoint,
        zoom: f64,
    ) -> Result<Self> {
        if !content.valid(u32::MAX, u32::MAX)
            || !content_origin.x.is_finite()
            || !content_origin.y.is_finite()
            || !zoom.is_finite()
            || zoom <= 0.
            || !(zoom as f32).is_finite()
            || zoom as f32 <= 0.
        {
            return Err("贴图外观预览几何无效".into());
        }
        let style = style.normalized();
        let layout = style.layout(content.width, content.height)?;
        let target = TargetGeometry::new(content, style, content_origin, zoom, zoom);
        let raster = raster_bounds(target.content_bounds())?;
        raster_bounds(PresentationBounds {
            left: content_origin.x - f64::from(layout.padding.left) * zoom,
            top: content_origin.y - f64::from(layout.padding.top) * zoom,
            width: f64::from(layout.output_width) * zoom,
            height: f64::from(layout.output_height) * zoom,
        })?;
        Ok(Self {
            content,
            #[cfg(test)]
            layout,
            target,
            raster,
        })
    }
    pub(crate) fn content_bounds(self) -> PresentationBounds {
        self.target.content_bounds()
    }
    #[cfg(test)]
    pub(crate) fn outer_bounds(self) -> PresentationBounds {
        let content = self.content_bounds();
        PresentationBounds {
            left: content.left - f64::from(self.layout.padding.left) * self.target.sx,
            top: content.top - f64::from(self.layout.padding.top) * self.target.sy,
            width: f64::from(self.layout.output_width) * self.target.sx,
            height: f64::from(self.layout.output_height) * self.target.sy,
        }
    }
    pub(crate) fn style(self) -> SelectionFinishStyle {
        self.target.style
    }
    pub(crate) fn contour(self, outward: f64) -> PresentationContour {
        self.target.contour(outward)
    }
    /// 贴图可见外缘：外侧边框取边框外沿，否则取内容轮廓；圆角与内容共用同一半径。
    pub(crate) fn outline(self) -> PresentationContour {
        let style = self.style();
        let outward = if style.has_border() && !style.border_inside {
            f64::from(style.border_thickness)
        } else {
            0.
        };
        self.target.border_contour(outward)
    }
    pub(crate) fn border_contours(self) -> (PresentationContour, PresentationContour) {
        let thickness = f64::from(self.style().border_thickness);
        if self.style().border_inside {
            (
                self.target.border_contour(0.),
                self.target.border_contour(-thickness),
            )
        } else {
            (
                self.target.border_contour(thickness),
                self.target.border_contour(0.),
            )
        }
    }
    #[cfg(test)]
    fn spans(self) -> (u32, u32, u32, u32) {
        if self.target.rounded.radius == 0. {
            return (0, 0, 0, 0);
        }
        let bounds = self.content_bounds();
        let rx = self.target.rounded.radius * self.target.sx;
        let ry = self.target.rounded.radius * self.target.sy;
        let left = ((bounds.left + rx).ceil() - f64::from(self.raster.left)).max(0.) as u32;
        let top = ((bounds.top + ry).ceil() - f64::from(self.raster.top)).max(0.) as u32;
        let right = (f64::from(self.raster.left) + f64::from(self.raster.width)
            - (bounds.left + bounds.width - rx).floor())
        .max(0.) as u32;
        let bottom = (f64::from(self.raster.top) + f64::from(self.raster.height)
            - (bounds.top + bounds.height - ry).floor())
        .max(0.) as u32;
        let left = left.min(self.raster.width.div_ceil(2));
        let top = top.min(self.raster.height.div_ceil(2));
        (
            left,
            top,
            right.min(self.raster.width - left),
            bottom.min(self.raster.height - top),
        )
    }
    #[cfg(test)]
    pub(crate) fn body_regions(self) -> Vec<PresentationRegion> {
        let (left, top, right, bottom) = self.spans();
        let r = self.raster;
        let middle = r.width - left - right;
        [
            PresentationRegion {
                left: r.left + left as i32,
                top: r.top,
                width: middle,
                height: top,
            },
            PresentationRegion {
                left: r.left,
                top: r.top + top as i32,
                width: r.width,
                height: r.height - top - bottom,
            },
            PresentationRegion {
                left: r.left + left as i32,
                top: r.top + (r.height - bottom) as i32,
                width: middle,
                height: bottom,
            },
        ]
        .into_iter()
        .filter(|r| r.width > 0 && r.height > 0)
        .collect()
    }
    #[cfg(test)]
    pub(crate) fn corner_regions(self) -> Vec<PresentationRegion> {
        let (left, top, right, bottom) = self.spans();
        let r = self.raster;
        [
            PresentationRegion {
                left: r.left,
                top: r.top,
                width: left,
                height: top,
            },
            PresentationRegion {
                left: r.left + (r.width - right) as i32,
                top: r.top,
                width: right,
                height: top,
            },
            PresentationRegion {
                left: r.left,
                top: r.top + (r.height - bottom) as i32,
                width: left,
                height: bottom,
            },
            PresentationRegion {
                left: r.left + (r.width - right) as i32,
                top: r.top + (r.height - bottom) as i32,
                width: right,
                height: bottom,
            },
        ]
        .into_iter()
        .filter(|r| r.width > 0 && r.height > 0)
        .collect()
    }
    /// 只画当前小角的内容+Scene+套索，真透明 alpha；阴影/边框由独立 GPUI path 绘制。
    #[cfg(test)]
    pub(crate) fn render_corner(
        self,
        source: &SourcePixels<'_>,
        scene: &Scene,
        mask: Option<&Polygon>,
        region: PresentationRegion,
    ) -> Result<Image> {
        if !self.corner_regions().contains(&region) {
            return Err("贴图圆角预览区域不属于当前四角".into());
        }
        self.render_content_region(source, scene, mask, region)
    }

    /// 内容/Scene/mask 的显示目标 ROI，使用线性采样和绝对客户区物理栅格的分数相位。
    /// ROI 可以跨出内容边缘，外侧按几何 alpha 返回透明；不合成装饰或窗口透明度。
    /// 所有有符号边界、4096 单边和 3200 万像素预算在分配前核验。
    pub(crate) fn render_content_region(
        self,
        source: &SourcePixels<'_>,
        scene: &Scene,
        mask: Option<&Polygon>,
        region: PresentationRegion,
    ) -> Result<Image> {
        validate_source(source, self.content)?;
        validate_preview_region(region)?;
        let projection = OutputProjection {
            source: self.content,
            width: region.width,
            height: region.height,
            sx: self.target.sx as f32,
            sy: self.target.sy as f32,
            origin: crate::annotation::Point {
                x: (f64::from(region.left) - self.target.origin.x) as f32,
                y: (f64::from(region.top) - self.target.origin.y) as f32,
            },
        };
        let mut image = display_content(source, scene, mask, projection)?;
        for y in 0..image.height {
            for x in 0..image.width {
                let p = PhysicalPoint {
                    x: f64::from(region.left) + f64::from(x) + 0.5,
                    y: f64::from(region.top) + f64::from(y) + 0.5,
                };
                let i = (y as usize * image.width as usize + x as usize) * 4;
                image.bgra[i + 3] =
                    (f64::from(image.bgra[i + 3]) * self.target.coverage(p)).round() as u8;
                if image.bgra[i + 3] == 0 {
                    image.bgra[i..i + 4].fill(0);
                }
            }
        }
        Ok(image)
    }

    /// 只预采样截图底图：不压入Scene、套索或圆角alpha。圆角在GPUI组最终合成时施加。
    pub(crate) fn render_unclipped_background(
        self,
        source: &SourcePixels<'_>,
        region: PresentationRegion,
    ) -> Result<Image> {
        validate_source(source, self.content)?;
        validate_preview_region(region)?;
        let projection = OutputProjection {
            source: self.content,
            width: region.width,
            height: region.height,
            sx: self.target.sx as f32,
            sy: self.target.sy as f32,
            origin: crate::annotation::Point {
                x: (f64::from(region.left) - self.target.origin.x) as f32,
                y: (f64::from(region.top) - self.target.origin.y) as f32,
            },
        };
        if self.target.sx == 1. && self.target.sy == 1. {
            if let Some(image) = read_unscaled_background(source, projection) {
                return Ok(image);
            }
        }
        display_content(source, &Scene::default(), None, projection)
    }
}

/// 整数 1:1 底图无需逐像素双线性采样；保留完整覆盖、叠层和透明色语义。
fn read_unscaled_background(
    source: &SourcePixels<'_>,
    projection: OutputProjection,
) -> Option<Image> {
    if projection.sx != 1.
        || projection.sy != 1.
        || !projection.origin.x.is_finite()
        || !projection.origin.y.is_finite()
        || projection.origin.x.fract() != 0.
        || projection.origin.y.fract() != 0.
        || projection.width == 0
        || projection.height == 0
    {
        return None;
    }
    let coordinate = |start: u32, offset: f32| {
        let value = f64::from(start) + f64::from(offset);
        (value >= 0. && value < f64::from(1_u32 << 23)).then_some(value as u32)
    };
    let region = Rect {
        x: coordinate(projection.source.x, projection.origin.x)?,
        y: coordinate(projection.source.y, projection.origin.y)?,
        width: projection.width,
        height: projection.height,
    };
    let right = region.x.checked_add(region.width)?;
    let bottom = region.y.checked_add(region.height)?;
    let canvas_right = source.canvas.x.checked_add(source.canvas.width)?;
    let canvas_bottom = source.canvas.y.checked_add(source.canvas.height)?;
    if right > 1_u32 << 23
        || bottom > 1_u32 << 23
        || region.x < source.canvas.x
        || region.y < source.canvas.y
        || right > canvas_right
        || bottom > canvas_bottom
    {
        return None;
    }
    // 使用当前 f32 投影核中心，不能让大原点的舍入借用整数拷贝资格。
    for (x, y) in [(0, 0), (region.width - 1, region.height - 1)] {
        if projection.source_center(x, y)
            != (crate::annotation::Point {
                x: (region.x + x) as f32 + 0.5,
                y: (region.y + y) as f32 + 0.5,
            })
        {
            return None;
        }
    }
    let planes = || source.planes.iter().chain(source.overlay.iter());
    for plane in planes() {
        let plane_right = plane.rect.x.checked_add(plane.rect.width)?;
        let plane_bottom = plane.rect.y.checked_add(plane.rect.height)?;
        let bytes = plane
            .rect
            .width
            .checked_mul(plane.rect.height)?
            .checked_mul(4)?;
        if plane.rect.width == 0
            || plane.rect.height == 0
            || plane_right > canvas_right
            || plane_bottom > canvas_bottom
            || plane.bgra.len() != bytes as usize
        {
            return None;
        }
    }
    if source.fill.is_none() {
        // 只扫描平面矩形的纵向分段；平面之间的空洞仍由原采样路径报错。
        let mut y = region.y;
        while y < bottom {
            let mut covered = region.x;
            while covered < right {
                let next = planes()
                    .filter(|plane| {
                        plane.rect.y <= y
                            && y < plane.rect.y + plane.rect.height
                            && plane.rect.x <= covered
                    })
                    .map(|plane| plane.rect.x + plane.rect.width)
                    .max()
                    .unwrap_or(covered);
                if next <= covered {
                    return None;
                }
                covered = next;
            }
            y = planes()
                .flat_map(|plane| [plane.rect.y, plane.rect.y + plane.rect.height])
                .filter(|edge| *edge > y)
                .min()
                .unwrap_or(bottom)
                .min(bottom);
        }
    }
    let mut image = source.read(region);
    // 单权重点的预乘插值会清掉 alpha=0 的隐藏 RGB，拷贝也必须一致。
    for pixel in image.bgra.chunks_exact_mut(4) {
        if pixel[3] == 0 {
            pixel.fill(0);
        }
    }
    Some(image)
}

/// 分配前预校验和实际ROI渲染共用同一块资格，避免接受了无法渲染的圆角几何。
fn validate_preview_region(region: PresentationRegion) -> Result<()> {
    if region.width == 0
        || region.height == 0
        || region.width > PREVIEW_TILE_SIDE
        || region.height > PREVIEW_TILE_SIDE
        || i64::from(region.left) + i64::from(region.width) > i64::from(i32::MAX)
        || i64::from(region.top) + i64::from(region.height) > i64::from(i32::MAX)
    {
        return Err("贴图目标内容区域为空、溢出或超过 4096 物理像素".into());
    }
    OutputProjection::new(Rect::full(region.width, region.height), 1.)?;
    Ok(())
}

fn raster_bounds(bounds: PresentationBounds) -> Result<PresentationRegion> {
    let (left, top, right, bottom) = (
        bounds.left.floor(),
        bounds.top.floor(),
        (bounds.left + bounds.width).ceil(),
        (bounds.top + bounds.height).ceil(),
    );
    if [left, top, right, bottom]
        .into_iter()
        .any(|v| !v.is_finite() || v < f64::from(i32::MIN) || v > f64::from(i32::MAX))
        || right <= left
        || bottom <= top
        || right - left > 65535.
        || bottom - top > 65535.
    {
        return Err("贴图预览物理栅格为空、溢出或超过单边上限".into());
    }
    Ok(PresentationRegion {
        left: left as i32,
        top: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}
fn validate_source(source: &SourcePixels<'_>, content: Rect) -> Result<()> {
    // 此入口只消费 Pin 当前内容图片，不消费整屏冻结纹理；限制也保证 f32 原图端点有效。
    OutputProjection::new(Rect::full(source.canvas.width, source.canvas.height), 1.)?;
    if !content.valid(source.canvas.width, source.canvas.height) {
        return Err("贴图外观内容超出底图".into());
    }
    Ok(())
}
fn projected_content(
    source: &SourcePixels<'_>,
    scene: &Scene,
    mask: Option<&Polygon>,
    projection: OutputProjection,
) -> Result<Image> {
    let len = (projection.width as usize)
        .checked_mul(projection.height as usize)
        .and_then(|v| v.checked_mul(4))
        .ok_or("贴图外观目标栅格溢出")?;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(len)?;
    bgra.resize(len, 0);
    for y in 0..projection.height {
        for x in 0..projection.width {
            let mut p = projection.source_center(x, y);
            // 边缘半像素覆盖使用选区内侧像素，不能采到 crop 外的其它桌面像素。
            p.x = p.x.clamp(
                projection.source.x as f32,
                ((u64::from(projection.source.x) + u64::from(projection.source.width)) as f32)
                    .next_down(),
            );
            p.y = p.y.clamp(
                projection.source.y as f32,
                ((u64::from(projection.source.y) + u64::from(projection.source.height)) as f32)
                    .next_down(),
            );
            let i = (y as usize * projection.width as usize + x as usize) * 4;
            bgra[i..i + 4].copy_from_slice(&source.sample(p));
        }
    }
    let mut image = Image {
        width: projection.width,
        height: projection.height,
        bgra,
        readback_ms: 0.,
    };
    scene.apply_projected(&mut image, source, &projection)?;
    if let Some(mask) = mask {
        mask.apply_projected(&mut image, &projection);
    }
    Ok(image)
}

/// 与当前 Canvas 原图线性显示及取色共用预乘插值，Scene 不走导出 1× 快捷路径。
/// 用户输出仍使用独立 projected_content，保留 nearest 和 canonical 1× 语义。
fn display_content(
    source: &SourcePixels<'_>,
    scene: &Scene,
    mask: Option<&Polygon>,
    projection: OutputProjection,
) -> Result<Image> {
    // 整块采样与逐像素 linear_pixel 逐字节一致；缩放时每帧都要按新倍率重采样。
    let mut image = crate::pin_color_sample::linear_background(source, &projection)?;
    scene.apply_display(&mut image, source, &projection)?;
    if let Some(mask) = mask {
        mask.apply_projected(&mut image, &projection);
    }
    Ok(image)
}
fn blend_pixel(destination: &mut [u8; 4], source: [u8; 4]) {
    let sa = u32::from(source[3]);
    if sa == 0 {
        return;
    }
    let da = u32::from(destination[3]);
    if sa == 255 || da == 0 {
        *destination = source;
        return;
    }
    let alpha = sa * 255 + da * (255 - sa);
    for c in 0..3 {
        let color = u32::from(source[c]) * sa * 255 + u32::from(destination[c]) * da * (255 - sa);
        destination[c] = ((color + alpha / 2) / alpha) as u8;
    }
    destination[3] = ((alpha + 127) / 255) as u8;
}

#[cfg(test)]
#[path = "pin_presentation_tests.rs"]
mod tests;
