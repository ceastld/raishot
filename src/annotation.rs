//! 全屏物理坐标中的标注；不依赖 GPUI、截图选区或撤销栈。
use crate::geometry::Rect;

#[path = "rich_text.rs"]
pub mod rich;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Crop,
    Edit,
    Rectangle,
    Ellipse,
    /// 三角形、菱形、五角星：按外接框生成的闭合直边多边形，描边/填充同椭圆。
    Triangle,
    Diamond,
    Star,
    Arrow,
    Pen,
    Mosaic,
    Highlighter,
    HighlighterRect,
    Polyline,
    Text,
    Watermark,
    Serial,
    Blur,
    MosaicBrush,
    BlurBrush,
    LocalFill,
    Spotlight,
    Eraser,
    PixelEraser,
    Lasso,
    /// 智能候选交互工具；文档效果仍使用 Mosaic / Blur。
    SmartMosaic,
}
impl Tool {
    pub fn draws(self) -> bool {
        !matches!(
            self,
            Self::Crop | Self::Edit | Self::Eraser | Self::Lasso | Self::SmartMosaic
        )
    }
    pub fn freehand(self) -> bool {
        matches!(
            self,
            Self::Pen
                | Self::Highlighter
                | Self::MosaicBrush
                | Self::BlurBrush
                | Self::PixelEraser
                | Self::LocalFill
        )
    }
    pub fn effect(self) -> bool {
        matches!(
            self,
            Self::Mosaic | Self::Blur | Self::MosaicBrush | Self::BlurBrush | Self::LocalFill
        )
    }
    pub fn text(self) -> bool {
        matches!(self, Self::Text | Self::Watermark)
    }
    /// 外接框决定几何的直边多边形形状。
    pub fn polygon(self) -> bool {
        matches!(self, Self::Triangle | Self::Diamond | Self::Star)
    }
    /// 可描边或填充的封闭形状（矩形、椭圆与多边形）。
    pub fn closed_shape(self) -> bool {
        matches!(self, Self::Rectangle | Self::Ellipse) || self.polygon()
    }
    /// 有圆角几何的区域工具统一开放参数、滚轮与对象拖柄。
    pub fn supports_radius(self) -> bool {
        matches!(
            self,
            Self::Rectangle | Self::HighlighterRect | Self::Spotlight | Self::Mosaic | Self::Blur
        )
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Crop => tr!("选区", "Selection"),
            Self::Edit => tr!("编辑", "Edit"),
            Self::Rectangle => tr!("矩形", "Rectangle"),
            Self::Ellipse => tr!("椭圆", "Ellipse"),
            Self::Triangle => tr!("三角形", "Triangle"),
            Self::Diamond => tr!("菱形", "Diamond"),
            Self::Star => tr!("五角星", "Star"),
            Self::Arrow => tr!("箭头", "Arrow"),
            Self::Pen => tr!("画笔", "Pen"),
            Self::Mosaic => tr!("马赛克", "Mosaic"),
            Self::Highlighter => tr!("荧光笔", "Highlighter"),
            Self::HighlighterRect => tr!("高亮框", "Highlight box"),
            Self::Polyline => tr!("折线", "Polyline"),
            Self::Text => tr!("文字", "Text"),
            Self::Watermark => tr!("水印", "Watermark"),
            Self::Serial => tr!("序号", "Number"),
            Self::Blur => tr!("模糊", "Blur"),
            Self::MosaicBrush => tr!("打码笔", "Mosaic brush"),
            Self::BlurBrush => tr!("模糊笔", "Blur brush"),
            Self::LocalFill => tr!("填充", "Fill"),
            Self::Spotlight => tr!("聚光灯", "Spotlight"),
            Self::Eraser => tr!("对象橡皮擦", "Object eraser"),
            Self::PixelEraser => tr!("局部橡皮擦", "Pixel eraser"),
            Self::Lasso => tr!("套索裁剪", "Lasso crop"),
            Self::SmartMosaic => tr!("智能马赛克", "Smart mosaic"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}
impl Point {
    pub fn from_tuple(p: (i32, i32)) -> Self {
        Self {
            x: p.0 as f32,
            y: p.1 as f32,
        }
    }
    pub fn rounded(self) -> (i32, i32) {
        (self.x.round() as i32, self.y.round() as i32)
    }
    pub fn within(self, canvas: Rect) -> Self {
        Self {
            x: self
                .x
                .clamp(canvas.x as f32, (canvas.x + canvas.width) as f32),
            y: self
                .y
                .clamp(canvas.y as f32, (canvas.y + canvas.height) as f32),
        }
    }
    pub fn distance(self, other: Self) -> f32 {
        (self.x - other.x).hypot(self.y - other.y)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}
impl Bounds {
    pub fn contains(self, p: Point, margin: f32) -> bool {
        p.x >= self.x - margin
            && p.x <= self.x + self.width + margin
            && p.y >= self.y - margin
            && p.y <= self.y + self.height + margin
    }
    pub fn pixels(self, canvas: Rect) -> Option<Rect> {
        let rect = Rect::drag(
            (self.x.floor() as i32, self.y.floor() as i32),
            (
                (self.x + self.width).ceil() as i32,
                (self.y + self.height).ceil() as i32,
            ),
            canvas.width,
            canvas.height,
        );
        rect.intersection(canvas)
    }
    pub fn handles(self) -> [Point; 8] {
        let (x, y, r, b) = (self.x, self.y, self.x + self.width, self.y + self.height);
        [
            Point { x, y },
            Point { x: (x + r) / 2., y },
            Point { x: r, y },
            Point { x, y: (y + b) / 2. },
            Point {
                x: r,
                y: (y + b) / 2.,
            },
            Point { x, y: b },
            Point {
                x: (x + r) / 2.,
                y: b,
            },
            Point { x: r, y: b },
        ]
    }
}

/// 旧样式缺少该字段时保留半透明合成；新工具默认值由 ToolPresets 决定。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HighlighterBlend {
    #[default]
    Alpha,
    Multiply,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HighlighterShape {
    #[default]
    Freehand,
    Line,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Style {
    pub color: u32,
    pub width: f32,
    pub block_size: u32,
    pub opacity: f32,
    pub filled: bool,
    pub radius: f32,
    /// 仅矩形使用的局部 TL/TR/BR/BL 圆角；None 保留旧联动模式。
    pub rectangle_radii: Option<[f32; 4]>,
    /// 普通矩形效果的显式圆角；旧 radius 对效果没有语义，不能继承其合法旧值。
    pub effect_radius: f32,
    pub font_size: f32,
    pub bold: bool,
    pub text: crate::text_format::TextStyle,
    pub arrow_head: crate::arrow_geometry::ArrowHead,
    pub arrow_start: bool,
    pub arrow_end: bool,
    pub dashed: bool,
    pub pencil_smooth: bool,
    pub polyline_smooth: bool,
    pub highlighter_blend: HighlighterBlend,
    pub highlighter_shape: HighlighterShape,
}
impl Default for Style {
    fn default() -> Self {
        Self {
            color: 0xff4b5c,
            width: 4.,
            block_size: 16,
            opacity: 1.,
            filled: false,
            radius: 0.,
            rectangle_radii: None,
            effect_radius: 0.,
            font_size: 28.,
            bold: false,
            text: Default::default(),
            arrow_head: Default::default(),
            arrow_start: false,
            arrow_end: true,
            dashed: false,
            pencil_smooth: true,
            polyline_smooth: false,
            highlighter_blend: HighlighterBlend::default(),
            highlighter_shape: HighlighterShape::default(),
        }
    }
}

impl Style {
    pub fn radius_for(self, tool: Tool) -> f32 {
        if tool == Tool::Rectangle {
            if let Some(radii) = self.rectangle_radii {
                return radii[0];
            }
        }
        if matches!(tool, Tool::Mosaic | Tool::Blur) {
            self.effect_radius
        } else {
            self.radius
        }
    }
    pub fn set_radius_for(&mut self, tool: Tool, value: f32) {
        if matches!(tool, Tool::Mosaic | Tool::Blur) {
            self.effect_radius = value;
        } else {
            self.radius = value;
            if tool == Tool::Rectangle && self.rectangle_radii.is_some() {
                self.rectangle_radii = Some([value; 4]);
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum LocalFillGeometry {
    #[default]
    Stroke,
    Regions(std::sync::Arc<[Bounds]>),
}

/// 一对象内的离散区域共享同一份下层像素；归属随对象增量历史撤销。
#[derive(Clone, Debug, PartialEq)]
pub struct EffectRegions {
    pub regions: std::sync::Arc<[Bounds]>,
    pub managed: bool,
}
impl EffectRegions {
    fn valid(&self) -> bool {
        !self.regions.is_empty()
            && self.regions.len() <= 4096
            && self.regions.iter().all(|r| {
                r.x.is_finite()
                    && r.y.is_finite()
                    && r.width.is_finite()
                    && r.height.is_finite()
                    && r.width > 0.
                    && r.height > 0.
                    && (r.x + r.width).is_finite()
                    && (r.y + r.height).is_finite()
            })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Annotation {
    pub tool: Tool,
    pub points: Vec<Point>,
    pub style: Style,
    pub text: String,
    pub rotation: f32,
    /// 同批区域共享下层采样基线，不以多个独立效果对象依次污染背景。
    pub fill_geometry: LocalFillGeometry,
    pub effect_regions: Option<EffectRegions>,
    /// 仅文字对象：局部样式片段与段落格式；None 时与整对象格式完全一致。
    pub rich: Option<std::sync::Arc<rich::RichText>>,
}

/// 一次解析的普通效果轮廓；逐像素采样复用逆变换，不重复计算旋转三角函数。
pub(crate) struct EffectGeometry {
    bounds: Bounds,
    inverse: crate::transform::Rotation,
    quarter_turn: Option<u8>,
    radius: f64,
}
impl EffectGeometry {
    pub(crate) fn contains(&self, point: Point, tolerance: f32) -> bool {
        if ![point.x, point.y, tolerance].iter().all(|v| v.is_finite()) {
            return false;
        }
        if self.radius == 0. {
            // 旧硬矩形保留原 f32 Rotation 和闭边界像素，不能因新增圆角迁移旧输出。
            return self.bounds.contains(self.inverse.map(point), tolerance);
        }
        let half_width = f64::from(self.bounds.width) / 2.;
        let half_height = f64::from(self.bounds.height) / 2.;
        let center_x = f64::from(self.bounds.x) + half_width;
        let center_y = f64::from(self.bounds.y) + half_height;
        let (x, y) = if let Some(quarter_turn) = self.quarter_turn {
            // 整角是精确交换/取反；sin_cos 残差会把边界像素误移到闭轮廓外。
            // 只消费确切整角，89.999 等角度仍走现有 Rotation，不作吸附或加容差。
            let dx = f64::from(point.x) - center_x;
            let dy = f64::from(point.y) - center_y;
            match quarter_turn {
                0 => (f64::from(point.x), f64::from(point.y)),
                1 => (center_x + dy, center_y - dx),
                2 => (center_x - dx, center_y - dy),
                _ => (center_x - dy, center_y + dx),
            }
        } else {
            let point = self.inverse.map(point);
            if ![point.x, point.y].iter().all(|v| v.is_finite()) {
                return false;
            }
            (f64::from(point.x), f64::from(point.y))
        };
        // signed distance 保留向外命中容差；覆盖仍是源像素中心的硬0/1掩模。
        let dx = (x - center_x).abs() - (half_width - self.radius);
        let dy = (y - center_y).abs() - (half_height - self.radius);
        let distance = dx.max(0.).hypot(dy.max(0.)) + dx.max(dy).min(0.) - self.radius;
        distance <= f64::from(tolerance)
    }
}

impl Annotation {
    pub fn supports_radius(&self) -> bool {
        self.tool.supports_radius()
            && (!matches!(self.tool, Tool::Mosaic | Tool::Blur) || self.effect_regions.is_none())
    }
    pub fn corner_radius(&self) -> f32 {
        self.style.radius_for(self.tool)
    }
    pub fn radius_at_corner(&self, corner: usize) -> f32 {
        if self.tool == Tool::Rectangle {
            if let Some(radii) = self.style.rectangle_radii {
                if let Some(&radius) = radii.get(corner) {
                    return radius;
                }
            }
        }
        self.corner_radius()
    }
    pub fn set_corner_radius(&mut self, value: f32) {
        self.style.set_radius_for(self.tool, value);
    }
    pub fn set_corner_handle_radius(&mut self, corner: usize, value: f32) {
        if corner >= 4 {
            return;
        }
        if self.tool == Tool::Rectangle {
            if let Some(radii) = &mut self.style.rectangle_radii {
                if radii[corner] == value {
                    return;
                }
                radii[corner] = value;
                self.style.radius = radii[0];
                return;
            }
        }
        self.set_corner_radius(value);
    }
    /// 效果像素与命中共用解析轮廓；旋转只在源坐标逆变换一次。
    pub(crate) fn effect_geometry(&self) -> Option<EffectGeometry> {
        if !matches!(self.tool, Tool::Mosaic | Tool::Blur) || self.effect_regions.is_some() {
            return None;
        }
        let bounds = self.local_bounds();
        let radius = self.corner_radius();
        if ![
            radius,
            self.rotation,
            bounds.x,
            bounds.y,
            bounds.width,
            bounds.height,
            bounds.x + bounds.width,
            bounds.y + bounds.height,
        ]
        .iter()
        .all(|v| v.is_finite())
            || bounds.width <= 0.
            || bounds.height <= 0.
        {
            return None;
        }
        let radius = radius.max(0.).min(bounds.width.min(bounds.height) / 2.);
        Some(EffectGeometry {
            bounds,
            inverse: self.rotation_transform().inverse(),
            quarter_turn: match self.rotation.rem_euclid(360.) {
                0. => Some(0),
                90. => Some(1),
                180. => Some(2),
                270. => Some(3),
                _ => None,
            },
            radius: f64::from(radius),
        })
    }
    pub fn new(tool: Tool, start: Point, style: Style) -> Self {
        Self {
            tool,
            points: if tool == Tool::Highlighter
                && style.highlighter_shape == HighlighterShape::Line
            {
                vec![start, start]
            } else {
                vec![start]
            },
            style,
            text: String::new(),
            rotation: 0.,
            fill_geometry: LocalFillGeometry::Stroke,
            effect_regions: None,
            rich: None,
        }
    }
    /// 对象级字体、颜色、字形开关改动等同全选后设置，清掉局部片段在该属性上的覆盖。
    /// 字号是基准，片段按倍数跟随，不清除。
    pub(crate) fn clear_rich_overrides(&mut self, before: Style, after: Style) {
        let Some(rich) = &mut self.rich else {
            return;
        };
        let changed = [
            (before.text.font != after.text.font, rich::Attribute::Font),
            (before.color != after.color, rich::Attribute::Color),
            (before.bold != after.bold, rich::Attribute::Bold),
            (
                before.text.italic != after.text.italic,
                rich::Attribute::Italic,
            ),
            (
                before.text.underline != after.text.underline,
                rich::Attribute::Underline,
            ),
            (
                before.text.strike != after.text.strike,
                rich::Attribute::Strike,
            ),
        ];
        if changed.iter().any(|(c, _)| *c) {
            let rich = std::sync::Arc::make_mut(rich);
            for (_, attribute) in changed.into_iter().filter(|(c, _)| *c) {
                rich.clear(attribute);
            }
        }
    }
    /// 输入为未旋转的全画布物理区域；不从识别 ROI 或当前鼠标推断坐标。
    pub fn set_effect_regions(&mut self, regions: Vec<Bounds>, managed: bool) -> crate::Result<()> {
        let geometry = EffectRegions {
            regions: regions.into(),
            managed,
        };
        if !matches!(self.tool, Tool::Mosaic | Tool::Blur) || !geometry.valid() {
            return Err("复合效果需要有效的马赛克/模糊区域（最多4096个）".into());
        }
        let bounds = crate::transform::bounds(
            geometry
                .regions
                .iter()
                .flat_map(|r| crate::transform::corners(*r)),
        );
        if !bounds.width.is_finite()
            || !bounds.height.is_finite()
            || bounds.width <= 0.
            || bounds.height <= 0.
        {
            return Err("复合效果区域范围无效或溢出".into());
        }
        self.points = vec![
            Point {
                x: bounds.x,
                y: bounds.y,
            },
            Point {
                x: bounds.x + bounds.width,
                y: bounds.y + bounds.height,
            },
        ];
        self.rotation = 0.;
        self.effect_regions = Some(geometry);
        Ok(())
    }
    pub fn effect_regions_managed(&self) -> bool {
        self.rotation == 0.
            && self.valid()
            && self
                .effect_regions
                .as_ref()
                .is_some_and(|g| g.managed && g.valid())
    }
    pub fn clear_effect_regions_management(&mut self) -> bool {
        let Some(geometry) = &mut self.effect_regions else {
            return false;
        };
        std::mem::replace(&mut geometry.managed, false)
    }
    pub fn effect_region_polygons(&self) -> Vec<[Point; 4]> {
        let transform = self.rotation_transform();
        self.effect_regions.as_ref().map_or_else(Vec::new, |g| {
            g.regions
                .iter()
                .map(|r| {
                    let quad = crate::transform::corners(*r);
                    if self.rotation == 0. {
                        quad
                    } else {
                        quad.map(|p| transform.map(p))
                    }
                })
                .collect()
        })
    }
    /// 旋转后的每块 AABB 仅供范围/UI投影；像素覆盖仍使用真实矩形的逆变换。
    pub fn effect_regions_world(&self) -> Vec<Bounds> {
        if self.rotation == 0. {
            return self
                .effect_regions
                .as_ref()
                .map_or_else(Vec::new, |g| g.regions.to_vec());
        }
        self.effect_region_polygons()
            .into_iter()
            .map(crate::transform::bounds)
            .collect()
    }
    pub fn is_highlighter_line(&self) -> bool {
        self.tool == Tool::Highlighter && self.style.highlighter_shape == HighlighterShape::Line
    }
    /// 两端点画笔编辑端点，不把逻辑外接框的角点当作 resize 节点。
    pub fn has_endpoint_handles(&self) -> bool {
        self.points.len() == 2 && (self.tool == Tool::Pen || self.is_highlighter_line())
    }
    pub fn update(&mut self, p: Point) {
        if self.is_highlighter_line() {
            if self.points.len() > 1 {
                self.points[1] = p;
                self.points.truncate(2);
            } else {
                self.points.push(p);
            }
        } else if self.tool.freehand() || self.tool == Tool::Polyline {
            let minimum = if self.tool == Tool::Pen { 0.25 } else { 1. };
            if self
                .points
                .last()
                .is_none_or(|last| last.distance(p) >= minimum)
            {
                self.points.push(p);
            }
        } else if self.points.len() > 1 {
            self.points[1] = p;
        } else {
            self.points.push(p);
        }
    }
    pub fn local_bounds(&self) -> Bounds {
        if let Some(geometry) = &self.effect_regions {
            return crate::transform::bounds(
                geometry
                    .regions
                    .iter()
                    .flat_map(|r| crate::transform::corners(*r)),
            );
        }
        if self.tool == Tool::LocalFill {
            if let LocalFillGeometry::Regions(regions) = &self.fill_geometry {
                return crate::transform::bounds(
                    regions.iter().flat_map(|r| crate::transform::corners(*r)),
                );
            }
        }
        if matches!(self.tool, Tool::Arrow | Tool::Polyline) {
            if let Some(bounds) = self.local_vector_path().bounds() {
                return bounds;
            }
        }
        let Some(&first) = self.points.first() else {
            return Bounds {
                x: 0.,
                y: 0.,
                width: 0.,
                height: 0.,
            };
        };
        let (mut x, mut y, mut right, mut bottom) = (first.x, first.y, first.x, first.y);
        for p in &self.points {
            x = x.min(p.x);
            y = y.min(p.y);
            right = right.max(p.x);
            bottom = bottom.max(p.y);
        }
        if self.tool == Tool::Serial {
            let radius = self.style.font_size * 0.8;
            return Bounds {
                x: first.x - radius,
                y: first.y - radius,
                width: radius * 2.,
                height: radius * 2.,
            };
        }
        Bounds {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
    pub fn rotation_transform(&self) -> crate::transform::Rotation {
        crate::transform::Rotation::new(
            crate::transform::center(self.local_bounds()),
            self.rotation,
        )
    }
    pub fn to_world(&self, p: Point) -> Point {
        if self.rotation == 0. {
            p
        } else {
            self.rotation_transform().map(p)
        }
    }
    pub fn to_local(&self, p: Point) -> Point {
        if self.rotation == 0. {
            p
        } else {
            self.rotation_transform().inverse().map(p)
        }
    }
    pub fn bounds(&self) -> Bounds {
        if self.rotation == 0. {
            return self.local_bounds();
        }
        if matches!(
            self.tool,
            Tool::Pen | Tool::Arrow | Tool::Polyline | Tool::Highlighter
        ) {
            if let Some(b) = self.vector_path().bounds() {
                return b;
            }
        }
        let t = self.rotation_transform();
        crate::transform::bounds(crate::transform::corners(self.local_bounds()).map(|p| t.map(p)))
    }
    /// 填充矩形与高亮框没有描边；保存的线宽仍供切回空心时使用。
    pub(crate) fn fill_only(&self) -> bool {
        self.tool == Tool::HighlighterRect || (self.tool == Tool::Rectangle && self.style.filled)
    }
    /// 绘制范围与逻辑选框分开：旋转圆角填充矩形按曲线取范围，不把透明角算成像素。
    /// 其它工具保持原有区域策略，缩放节点仍使用 bounds/local_bounds。
    pub(crate) fn render_bounds(&self) -> Bounds {
        if self.tool == Tool::Rectangle && self.style.filled {
            self.vector_path().bounds().unwrap_or_else(|| self.bounds())
        } else {
            self.bounds()
        }
    }
    pub fn rotate_about(&mut self, pivot: Point, degrees: f32) {
        if !degrees.is_finite() || degrees.abs() < 0.0001 {
            return;
        }
        if self.effect_regions.is_some() && (!pivot.x.is_finite() || !pivot.y.is_finite()) {
            return;
        }
        self.clear_effect_regions_management();
        let center = crate::transform::center(self.local_bounds());
        let next = crate::transform::Rotation::new(pivot, degrees).map(center);
        self.translate(next.x - center.x, next.y - center.y);
        self.rotation = (self.rotation + degrees + 180.).rem_euclid(360.) - 180.;
        if self.rotation.abs() < 0.0001 {
            self.rotation = 0.;
        }
    }
    pub fn scale_about(&mut self, pivot: Point, scale: f32) {
        if self.effect_regions.is_some() && scale == 1. {
            return;
        }
        if self.effect_regions.is_some()
            && (!scale.is_finite() || scale <= 0. || !pivot.x.is_finite() || !pivot.y.is_finite())
        {
            return;
        }
        if let Some(geometry) = &mut self.effect_regions {
            for r in std::sync::Arc::make_mut(&mut geometry.regions) {
                r.x = pivot.x + (r.x - pivot.x) * scale;
                r.y = pivot.y + (r.y - pivot.y) * scale;
                r.width *= scale;
                r.height *= scale;
            }
            if scale != 1. {
                geometry.managed = false;
            }
        }
        for p in &mut self.points {
            p.x = pivot.x + (p.x - pivot.x) * scale;
            p.y = pivot.y + (p.y - pivot.y) * scale;
        }
        // 聚光灯边框属于共享暗层，单独孔的几何缩放只改变孔与圆角。
        if self.tool != Tool::Spotlight {
            self.style.width *= scale;
        }
        if self.tool.text() || self.tool == Tool::Serial {
            self.style.font_size *= scale;
            self.style.text.outline_width *= scale;
            self.style.text.background_radius *= scale;
        }
        self.style.radius *= scale;
        if let Some(radii) = &mut self.style.rectangle_radii {
            for radius in radii {
                *radius *= scale;
            }
        }
        self.style.effect_radius *= scale;
    }
    pub fn preserve_rotation_frame(&mut self, old_center: Point) {
        if self.rotation == 0. {
            return;
        }
        let next = crate::transform::center(self.local_bounds());
        let mapped = crate::transform::Rotation::new(old_center, self.rotation).map(next);
        self.translate(mapped.x - next.x, mapped.y - next.y);
    }
    pub fn valid(&self) -> bool {
        if let Some(rich) = &self.rich {
            if self.tool != Tool::Text || rich.validate(&self.text).is_err() {
                return false;
            }
        }
        if self.tool == Tool::Rectangle
            && self
                .style
                .rectangle_radii
                .is_some_and(|r| r.iter().any(|v| !v.is_finite() || *v < 0.))
        {
            return false;
        }
        if let Some(geometry) = &self.effect_regions {
            return matches!(self.tool, Tool::Mosaic | Tool::Blur)
                && geometry.valid()
                && self.rotation.is_finite()
                && self.style.opacity.is_finite()
                && (0. ..=1.).contains(&self.style.opacity)
                && {
                    let b = self.local_bounds();
                    b.width.is_finite()
                        && b.height.is_finite()
                        && b.width > 0.
                        && b.height > 0.
                        && self.effect_region_polygons().iter().all(|quad| {
                            let world = crate::transform::bounds(*quad);
                            quad.iter().all(|p| p.x.is_finite() && p.y.is_finite())
                                && world.width.is_finite()
                                && world.height.is_finite()
                        })
                };
        }
        if self.tool == Tool::LocalFill {
            return match &self.fill_geometry {
                LocalFillGeometry::Stroke => {
                    !self.points.is_empty()
                        && self.style.width.is_finite()
                        && (1. ..=50.).contains(&self.style.width)
                        && self
                            .points
                            .iter()
                            .all(|p| p.x.is_finite() && p.y.is_finite())
                }
                LocalFillGeometry::Regions(regions) => {
                    !regions.is_empty()
                        && regions.iter().all(|r| {
                            r.x.is_finite()
                                && r.y.is_finite()
                                && r.width.is_finite()
                                && r.height.is_finite()
                                && r.width > 0.
                                && r.height > 0.
                                && (r.x + r.width).is_finite()
                                && (r.y + r.height).is_finite()
                        })
                }
            };
        }
        if !self.tool.draws() || self.points.is_empty() {
            return false;
        }
        let b = self.local_bounds();
        match self.tool {
            t if t.freehand() => true,
            Tool::Serial => true,
            Tool::Text | Tool::Watermark => !self.text.trim().is_empty(),
            Tool::Polyline => self.points.len() > 1 && (b.width >= 2. || b.height >= 2.),
            Tool::Arrow => self
                .points
                .get(1)
                .is_some_and(|b| self.points[0].distance(*b) >= 2.),
            _ => b.width >= 2. && b.height >= 2.,
        }
    }
    pub fn translate(&mut self, dx: f32, dy: f32) {
        if self.effect_regions.is_some() && (!dx.is_finite() || !dy.is_finite()) {
            return;
        }
        if let Some(geometry) = &mut self.effect_regions {
            for r in std::sync::Arc::make_mut(&mut geometry.regions) {
                r.x += dx;
                r.y += dy;
            }
            if dx != 0. || dy != 0. {
                geometry.managed = false;
            }
        }
        for p in &mut self.points {
            p.x += dx;
            p.y += dy;
        }
    }
    pub fn resized(&self, bounds: Bounds) -> Self {
        let old = self.local_bounds();
        if self.effect_regions.is_some() && bounds == old {
            return self.clone();
        }
        let mut result = self.clone();
        if let Some(geometry) = &mut result.effect_regions {
            if !bounds.x.is_finite()
                || !bounds.y.is_finite()
                || !bounds.width.is_finite()
                || !bounds.height.is_finite()
                || bounds.width <= 0.
                || bounds.height <= 0.
            {
                return self.clone();
            }
            for r in std::sync::Arc::make_mut(&mut geometry.regions) {
                r.x = bounds.x + (r.x - old.x) / old.width.max(f32::EPSILON) * bounds.width;
                r.y = bounds.y + (r.y - old.y) / old.height.max(f32::EPSILON) * bounds.height;
                r.width *= bounds.width / old.width.max(f32::EPSILON);
                r.height *= bounds.height / old.height.max(f32::EPSILON);
            }
            if bounds != old {
                geometry.managed = false;
            }
        }
        if self.tool == Tool::Serial {
            result.points[0] = Point {
                x: bounds.x + bounds.width / 2.,
                y: bounds.y + bounds.height / 2.,
            };
            result.style.font_size = bounds.width.min(bounds.height) / 1.6;
            result.preserve_rotation_frame(crate::transform::center(old));
            return result;
        }
        if self.tool.text() {
            result.style.font_size *= (bounds.height / old.height.max(1.)).clamp(0.25, 4.);
        }
        let minimum = if result.effect_regions.is_some() {
            f32::EPSILON
        } else {
            1.
        };
        for p in &mut result.points {
            p.x = bounds.x + (p.x - old.x) / old.width.max(minimum) * bounds.width;
            p.y = bounds.y + (p.y - old.y) / old.height.max(minimum) * bounds.height;
        }
        result.preserve_rotation_frame(crate::transform::center(old));
        result
    }
    pub fn handles(&self) -> Vec<Point> {
        let local = if self.tool == Tool::Arrow {
            self.points.get(1).map_or_else(Vec::new, |b| {
                vec![self.points[0], *b, self.arrow_midpoint()]
            })
        } else if self.tool == Tool::Polyline || self.has_endpoint_handles() {
            self.points.clone()
        } else {
            self.local_bounds().handles().to_vec()
        };
        let t = self.rotation_transform();
        local.into_iter().map(|p| t.map(p)).collect()
    }
    /// 独立于 resize/node 索引的四角统一半径拖柄；容差与现有屏幕节点同源。
    pub fn radius_controls(&self, tolerance: f32) -> Vec<crate::radius_handle::RadiusControl> {
        crate::radius_handle::controls(self, tolerance)
    }
    #[cfg(test)]
    pub fn radius_after_delta(&self, delta: f32) -> f32 {
        crate::radius_handle::radius_after_delta(self, delta)
    }
    pub fn radius_after_corner_delta(&self, corner: usize, delta: f32) -> f32 {
        crate::radius_handle::radius_after_corner_delta(self, corner, delta)
    }
    pub fn hit(&self, p: Point, tolerance: f32, interior: bool) -> bool {
        if let Some(geometry) = &self.effect_regions {
            let p = self.to_local(p);
            return geometry.regions.iter().any(|r| r.contains(p, tolerance));
        }
        // 参考填充提交后不可选中，保持连续绘制；批量ROI同样只随文档撤销。
        // 水印铺满整块画布，点哪都会命中；只经水印工具编辑，不进入选择框和旋转柄。
        if matches!(self.tool, Tool::LocalFill | Tool::Watermark) {
            return false;
        }
        if matches!(self.tool, Tool::Mosaic | Tool::Blur) {
            return self
                .effect_geometry()
                .is_some_and(|g| g.contains(p, tolerance));
        }
        let p = self.to_local(p);
        let half_width = if self.fill_only() {
            0.
        } else {
            self.style.width / 2.
        };
        if !self.local_bounds().contains(p, tolerance + half_width) {
            return false;
        }
        if self.tool == Tool::Serial {
            return p.distance(self.points[0]) <= self.style.font_size * 0.8 + tolerance;
        }
        if self.tool == Tool::Ellipse && (interior || self.style.filled) {
            let b = self.local_bounds();
            let rx = b.width / 2.;
            let ry = b.height / 2.;
            if rx > 0.
                && ry > 0.
                && ((p.x - b.x - rx) / rx).powi(2) + ((p.y - b.y - ry) / ry).powi(2) <= 1.
            {
                return true;
            }
            return self
                .local_segments()
                .iter()
                .any(|&(a, b)| distance_to_segment(p, a, b) <= tolerance + self.style.width / 2.);
        }
        if self.tool == Tool::Spotlight
            || self.tool == Tool::HighlighterRect
            || ((self.tool == Tool::Rectangle || self.tool.polygon())
                && (interior || self.style.filled))
        {
            let segments = self.local_segments();
            return contains_path(p, &segments)
                || segments
                    .iter()
                    .any(|&(a, b)| distance_to_segment(p, a, b) <= tolerance + half_width);
        }
        if self.tool.text() || self.tool == Tool::Serial {
            return self.local_bounds().contains(p, tolerance);
        }
        if self.tool == Tool::Arrow {
            let path = self.local_vector_path();
            if path.fills.iter().any(|contour| {
                let edges = crate::vector_path::contour_edges(contour);
                contains_path(p, &edges)
                    || edges
                        .iter()
                        .any(|&(a, b)| distance_to_segment(p, a, b) <= tolerance)
            }) {
                return true;
            }
        }
        // 拟合曲线与原始样本折线双向相距不超过约 1.25 px（降噪 0.75 + 简化 0.9 + 曲线 0.15）。
        // 悬停每次移动都会命中测试，先用样本折线粗判，只有贴近边缘时才重新拟合整笔。
        if self.tool == Tool::Pen && self.style.pencil_smooth && self.points.len() > 1 {
            const SLACK: f32 = 2.;
            let reach = tolerance + self.style.width / 2.;
            let near = self
                .points
                .windows(2)
                .map(|w| distance_to_segment(p, w[0], w[1]))
                .fold(f32::INFINITY, f32::min);
            if near > reach + SLACK {
                return false;
            }
            if near < reach - SLACK {
                return true;
            }
        }
        let segments = self.local_segments();
        (self.tool == Tool::Polyline
            && self.style.filled
            && self.points.len() >= 3
            && contains_path(p, &segments))
            || segments
                .iter()
                .any(|&(a, b)| distance_to_segment(p, a, b) <= tolerance + self.style.width / 2.)
    }
    fn local_vector_path(&self) -> crate::vector_path::VectorPath {
        use crate::vector_path::VectorPath;
        match self.tool {
            Tool::Pen | Tool::PixelEraser | Tool::LocalFill => {
                if self.style.pencil_smooth {
                    crate::pencil::fit(&self.points)
                } else {
                    VectorPath::from_polyline(&self.points)
                }
            }
            Tool::Arrow => self.points.get(1).map_or_else(VectorPath::default, |b| {
                VectorPath::arrow(self.points[0], *b, self.arrow_midpoint(), self.style)
            }),
            Tool::Polyline => {
                let mut path = VectorPath::polyline(
                    &self.points,
                    self.style.polyline_smooth,
                    self.style.filled,
                );
                if self.style.filled {
                    path.fills
                        .push(path.flattened().iter().map(|&(a, _)| a).collect());
                }
                path
            }
            Tool::Rectangle | Tool::HighlighterRect => {
                let mut path = if self.tool == Tool::Rectangle {
                    self.style.rectangle_radii.map_or_else(
                        || VectorPath::rectangle(self.local_bounds(), self.style.radius),
                        |radii| VectorPath::rectangle_corners(self.local_bounds(), radii),
                    )
                } else {
                    VectorPath::rectangle(self.local_bounds(), self.style.radius)
                };
                if self.style.filled || self.tool == Tool::HighlighterRect {
                    path.fills
                        .push(path.flattened().iter().map(|&(a, _)| a).collect());
                }
                path
            }
            Tool::Ellipse => {
                let mut path = VectorPath::ellipse(self.local_bounds());
                if self.style.filled {
                    path.fills
                        .push(path.flattened().iter().map(|&(a, _)| a).collect());
                }
                path
            }
            Tool::Triangle | Tool::Diamond | Tool::Star => {
                let corners = polygon_corners(self.tool, self.local_bounds());
                let mut path = VectorPath::polyline(&corners, false, true);
                if self.style.filled {
                    path.fills.push(corners);
                }
                path
            }
            Tool::Highlighter => VectorPath::from_polyline(&self.points),
            _ => VectorPath::default(),
        }
    }
    pub fn vector_path(&self) -> crate::vector_path::VectorPath {
        let mut path = self.local_vector_path();
        if self.rotation == 0. {
            return path;
        }
        let t = self.rotation_transform();
        for span in &mut path.spans {
            use crate::vector_path::Span;
            *span = match *span {
                Span::Line { start, end } => Span::Line {
                    start: t.map(start),
                    end: t.map(end),
                },
                Span::Cubic {
                    start,
                    control1,
                    control2,
                    end,
                } => Span::Cubic {
                    start: t.map(start),
                    control1: t.map(control1),
                    control2: t.map(control2),
                    end: t.map(end),
                },
            };
        }
        for contour in &mut path.fills {
            for p in contour {
                *p = t.map(*p);
            }
        }
        path
    }
    pub fn arrow_midpoint(&self) -> Point {
        self.points.get(2).copied().unwrap_or_else(|| {
            let a = self.points[0];
            let b = self.points.get(1).copied().unwrap_or(a);
            Point {
                x: (a.x + b.x) / 2.,
                y: (a.y + b.y) / 2.,
            }
        })
    }
    pub fn move_arrow_handle(&mut self, handle: usize, p: Point) {
        if self.points.len() < 2 {
            return;
        }
        if handle == 2 {
            if self.points.len() < 3 {
                self.points.push(p);
            } else {
                self.points[2] = p;
            }
        } else if handle < 2 {
            let a = self.points[0];
            let b = self.points[1];
            let mid = self.arrow_midpoint();
            self.points[handle] = p;
            if self.points.len() >= 3 {
                let (dx, dy) = (b.x - a.x, b.y - a.y);
                let len2 = dx * dx + dy * dy;
                let (nx, ny) = (
                    self.points[1].x - self.points[0].x,
                    self.points[1].y - self.points[0].y,
                );
                let (u, v) = if len2 > 0.0001 {
                    (
                        ((mid.x - a.x) * dx + (mid.y - a.y) * dy) / len2,
                        (-(mid.x - a.x) * dy + (mid.y - a.y) * dx) / len2,
                    )
                } else {
                    (0.5, 0.)
                };
                self.points[2] = Point {
                    x: self.points[0].x + u * nx - v * ny,
                    y: self.points[0].y + u * ny + v * nx,
                };
            }
        }
    }
    pub fn snap_arrow_midpoint(&mut self) {
        if self.points.len() < 3 {
            return;
        }
        let center = crate::transform::center(self.local_bounds());
        let (a, b) = (self.points[0], self.points[1]);
        if self.points[2].distance(Point {
            x: (a.x + b.x) / 2.,
            y: (a.y + b.y) / 2.,
        }) <= 6.
        {
            self.points.truncate(2);
            self.preserve_rotation_frame(center);
        }
    }
    pub fn node_insertion(&self, p: Point, tolerance: f32) -> Option<(usize, Point)> {
        let p = self.to_local(p);
        if self.tool != Tool::Polyline || self.points.iter().any(|v| v.distance(p) <= tolerance) {
            return None;
        }
        self.local_vector_path()
            .spans
            .iter()
            .enumerate()
            .flat_map(|(i, span)| {
                crate::vector_path::VectorPath {
                    spans: vec![*span],
                    ..Default::default()
                }
                .flattened()
                .into_iter()
                .map(move |(a, b)| {
                    let (dx, dy) = (b.x - a.x, b.y - a.y);
                    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy)
                        / (dx * dx + dy * dy).max(0.0001))
                    .clamp(0., 1.);
                    let q = Point {
                        x: a.x + t * dx,
                        y: a.y + t * dy,
                    };
                    (i + 1, q, p.distance(q))
                })
            })
            .min_by(|a, b| a.2.total_cmp(&b.2))
            .filter(|(_, _, d)| *d <= tolerance + self.style.width / 2.)
            .map(|(i, q, _)| (i, q))
    }
    pub fn segments(&self) -> Vec<(Point, Point)> {
        let segments = self.local_segments();
        if self.rotation == 0. {
            return segments;
        }
        let t = self.rotation_transform();
        segments
            .into_iter()
            .map(|(a, b)| (t.map(a), t.map(b)))
            .collect()
    }
    fn local_segments(&self) -> Vec<(Point, Point)> {
        let Some(&first) = self.points.first() else {
            return vec![];
        };
        match self.tool {
            Tool::Mosaic | Tool::Blur if self.effect_regions.is_none() => {
                crate::vector_path::VectorPath::rectangle(self.local_bounds(), self.corner_radius())
                    .flattened()
            }
            Tool::Spotlight => {
                crate::vector_path::VectorPath::rectangle(self.local_bounds(), self.style.radius)
                    .flattened()
            }
            Tool::Rectangle
            | Tool::HighlighterRect
            | Tool::Ellipse
            | Tool::Triangle
            | Tool::Diamond
            | Tool::Star => self.local_vector_path().flattened(),
            Tool::Arrow | Tool::Pen | Tool::Polyline | Tool::PixelEraser => {
                self.local_vector_path().flattened()
            }
            t if t.freehand() && self.points.len() == 1 => vec![(
                first,
                Point {
                    x: first.x + 0.01,
                    y: first.y,
                },
            )],
            t if t.freehand() => self.points.windows(2).map(|p| (p[0], p[1])).collect(),
            _ => vec![],
        }
    }
}
/// 多边形顶点按外接框比例生成，顺时针从顶点开始；旋转由调用方的标注变换处理。
pub fn polygon_corners(tool: Tool, b: Bounds) -> Vec<Point> {
    let at = |fx: f32, fy: f32| Point {
        x: b.x + b.width * fx,
        y: b.y + b.height * fy,
    };
    match tool {
        Tool::Triangle => vec![at(0.5, 0.), at(1., 1.), at(0., 1.)],
        Tool::Diamond => vec![at(0.5, 0.), at(1., 0.5), at(0.5, 1.), at(0., 0.5)],
        Tool::Star => {
            // 单位圆上正五角星的外接框：上顶点 y=-1、下两角 y=sin54°、左右 x=±cos18°；
            // 两轴分别缩放到外接框，内外半径比 0.382。
            let (top, bottom) = (-1., 54f32.to_radians().sin());
            let (left, right) = (-(18f32.to_radians().cos()), 18f32.to_radians().cos());
            (0..10)
                .map(|i| {
                    let angle = (-90. + 36. * i as f32).to_radians();
                    let r = if i % 2 == 0 { 1. } else { 0.381_966 };
                    let (x, y) = (r * angle.cos(), r * angle.sin());
                    at((x - left) / (right - left), (y - top) / (bottom - top))
                })
                .collect()
        }
        _ => Vec::new(),
    }
}
pub fn distance_to_segment(p: Point, a: Point, b: Point) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length = dx * dx + dy * dy;
    let t = if length > 0. {
        ((p.x - a.x) * dx + (p.y - a.y) * dy) / length
    } else {
        0.
    }
    .clamp(0., 1.);
    (p.x - a.x - t * dx).hypot(p.y - a.y - t * dy)
}

// 奇偶填充共用轮廓，凹多边形和自交区域的命中不退化成外接矩形。
pub fn contains_path(p: Point, segments: &[(Point, Point)]) -> bool {
    let mut inside = false;
    for &(a, b) in segments {
        if (a.y > p.y) != (b.y > p.y) && p.x < (b.x - a.x) * (p.y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangle_independent_corners_share_fill_and_hit_geometry_after_rotation() {
        let mut a = Annotation::new(
            Tool::Rectangle,
            Point { x: 20., y: 30. },
            Style {
                radius: 40.,
                rectangle_radii: Some([40., 0., 0., 0.]),
                filled: true,
                ..Default::default()
            },
        );
        a.update(Point { x: 220., y: 150. });
        let local_path = a.local_vector_path();
        assert_eq!(local_path.fills.len(), 1);
        assert_eq!(
            local_path.fills[0],
            local_path
                .flattened()
                .iter()
                .map(|&(p, _)| p)
                .collect::<Vec<_>>()
        );
        let outside_round_corner = Point { x: 22., y: 32. };
        let inside_square_corner = Point { x: 218., y: 148. };
        assert!(!a.hit(outside_round_corner, 0., true));
        assert!(a.hit(inside_square_corner, 0., true));
        a.rotation = 37.;
        assert!(!a.hit(a.to_world(outside_round_corner), 0., true));
        assert!(a.hit(a.to_world(inside_square_corner), 0., true));
        let world_path = a.vector_path();
        assert_eq!(
            world_path.spans.first().unwrap().start(),
            world_path.spans.last().unwrap().end()
        );
        assert_eq!(world_path.fills[0][0], a.to_world(local_path.fills[0][0]));

        for tool in [
            Tool::HighlighterRect,
            Tool::Spotlight,
            Tool::Mosaic,
            Tool::Blur,
        ] {
            let mut legacy = a.clone();
            legacy.tool = tool;
            legacy.style.rectangle_radii = None;
            let mut ignored = legacy.clone();
            ignored.style.rectangle_radii = Some([1., 200., 3., 300.]);
            assert_eq!(ignored.vector_path(), legacy.vector_path());
            assert_eq!(ignored.segments(), legacy.segments());
            assert_eq!(ignored.radius_controls(6.), legacy.radius_controls(6.));
            for corner in 0..4 {
                assert_eq!(ignored.radius_at_corner(corner), legacy.corner_radius());
            }
        }
    }

    #[test]
    fn rectangle_corner_setters_preserve_mode_and_unified_style_aliases() {
        let mut a = Annotation::new(Tool::Rectangle, Point { x: 0., y: 0. }, Style::default());
        a.update(Point { x: 200., y: 120. });
        let legacy_path = crate::vector_path::VectorPath::rectangle(a.local_bounds(), 0.);
        assert_eq!(a.local_vector_path(), legacy_path);
        a.set_corner_handle_radius(2, 12.);
        assert_eq!(a.style.rectangle_radii, None);
        assert_eq!(a.corner_radius(), 12.);
        for corner in 0..4 {
            assert_eq!(a.radius_at_corner(corner), 12.);
            assert_eq!(
                a.radius_after_corner_delta(corner, 3.),
                a.radius_after_delta(3.)
            );
        }
        a.style.rectangle_radii = Some([80., 10., 20., 30.]);
        assert_eq!(a.style.radius_for(Tool::Rectangle), 80.);
        let points = a.points.clone();
        a.set_corner_handle_radius(2, 24.);
        assert_eq!(a.style.rectangle_radii, Some([80., 10., 24., 30.]));
        assert_eq!(a.style.radius, 80.);
        assert_eq!(a.points, points);
        let before = a.clone();
        a.set_corner_handle_radius(4, 99.);
        assert_eq!(a, before);
        assert_eq!(a.radius_after_corner_delta(4, 99.), a.corner_radius());
        a.style.set_radius_for(Tool::Rectangle, 17.);
        assert_eq!(a.style.rectangle_radii, Some([17.; 4]));
        assert_eq!(a.style.radius, 17.);
        a.set_corner_radius(21.);
        assert_eq!(a.style.rectangle_radii, Some([21.; 4]));
        a.style.set_radius_for(Tool::Mosaic, 9.);
        assert_eq!(a.style.effect_radius, 9.);
        assert_eq!(a.style.rectangle_radii, Some([21.; 4]));
    }

    #[test]
    fn rectangle_independent_radii_scale_without_clamping_stored_large_values() {
        let mut a = Annotation::new(
            Tool::Rectangle,
            Point { x: 20., y: 30. },
            Style {
                radius: 80.,
                rectangle_radii: Some([80., 12., 4., 0.]),
                ..Default::default()
            },
        );
        a.update(Point { x: 220., y: 150. });
        let before = a.vector_path();
        assert_eq!(
            before.spans.first().unwrap().start(),
            Point { x: 80., y: 30. }
        );
        a.scale_about(Point { x: 0., y: 0. }, 2.);
        assert_eq!(a.style.rectangle_radii, Some([160., 24., 8., 0.]));
        assert_eq!(a.style.radius, 160.);
        assert_eq!(a.radius_at_corner(0), 160.);
        let after = a.vector_path();
        assert_eq!(before.spans.len(), after.spans.len());
        for (before, after) in before.spans.iter().zip(&after.spans) {
            for (before, after) in [(before.start(), after.start()), (before.end(), after.end())] {
                assert!((after.x - before.x * 2.).abs() < 0.001);
                assert!((after.y - before.y * 2.).abs() < 0.001);
            }
        }
        let mut linked = a.clone();
        linked.style.rectangle_radii = None;
        linked.scale_about(Point { x: 0., y: 0. }, 0.5);
        assert_eq!(linked.style.rectangle_radii, None);
        assert_eq!(linked.style.radius, 80.);
        assert_eq!(
            linked.vector_path(),
            crate::vector_path::VectorPath::rectangle(linked.local_bounds(), 80.)
        );
    }

    #[test]
    fn highlighter_line_model_preserves_legacy_defaults_and_zero_axis_geometry() {
        let legacy = Style::default();
        assert_eq!(legacy.highlighter_blend, HighlighterBlend::Alpha);
        assert_eq!(legacy.highlighter_shape, HighlighterShape::Freehand);
        let style = Style {
            highlighter_shape: HighlighterShape::Line,
            width: 12.,
            ..legacy
        };
        let start = Point { x: 40., y: 60. };
        let mut line = Annotation::new(Tool::Highlighter, start, style);
        assert!(line.valid());
        assert!(line.has_endpoint_handles());
        assert!(line.hit(start, 0., false));
        for end in [
            Point { x: 150., y: 60. },
            Point { x: 40., y: 150. },
            Point { x: 150., y: 150. },
        ] {
            line.update(end);
            assert_eq!(line.points, vec![start, end]);
            assert!(line.valid());
            assert_eq!(line.handles(), vec![start, end]);
            let center = Point {
                x: (start.x + end.x) / 2.,
                y: (start.y + end.y) / 2.,
            };
            assert!(line.hit(center, 0., false));
        }
        assert!(!line.hit(Point { x: 40., y: 150. }, 0., true));
        line.rotation = 31.;
        let handles = line.handles();
        assert!(line.hit(handles[0], 0.1, false));
        assert!(line.hit(handles[1], 0.1, false));
        let resized = line.resized(Bounds {
            x: 60.,
            y: 80.,
            width: 180.,
            height: 100.,
        });
        assert_eq!(resized.points.len(), 2);
        assert!(resized.has_endpoint_handles());
        assert_eq!(resized.style.highlighter_shape, HighlighterShape::Line);

        let mut freehand = Annotation::new(Tool::Highlighter, start, legacy);
        freehand.update(Point { x: 80., y: 70. });
        freehand.update(Point { x: 90., y: 120. });
        assert_eq!(freehand.points.len(), 3);
        assert!(!freehand.has_endpoint_handles());
    }
    #[test]
    fn pencil_hit_sample_shortcut_matches_fitted_geometry() {
        let near = |p: Point, lines: &[(Point, Point)]| {
            lines
                .iter()
                .map(|&(a, b)| distance_to_segment(p, a, b))
                .fold(f32::INFINITY, f32::min)
        };
        for seed in 1..=4 {
            let samples = crate::vector_mesh::tests::hand_samples(60., 60., 900, seed);
            let mut pen = Annotation::new(Tool::Pen, samples[0], Style::default());
            for &p in &samples[1..] {
                pen.update(p);
            }
            let fitted = pen.local_segments();
            let raw: Vec<_> = pen.points.windows(2).map(|w| (w[0], w[1])).collect();
            // 粗判依据：拟合曲线与样本折线双向相距明显小于 2 px 的余量。
            for &(a, b) in &fitted {
                for k in 0..=4 {
                    let t = k as f32 / 4.;
                    let p = Point {
                        x: a.x + (b.x - a.x) * t,
                        y: a.y + (b.y - a.y) * t,
                    };
                    assert!(near(p, &raw) < 1.5, "fitted {p:?} drifted from samples");
                }
            }
            for &p in &pen.points {
                assert!(near(p, &fitted) < 1.5, "sample {p:?} far from fitted path");
            }
            // 粗判给出的结论与按拟合线段的精确判定一致。
            let b = pen.bounds();
            for tolerance in [0., 6.] {
                let reach = tolerance + pen.style.width / 2.;
                let mut y = b.y - 10.;
                while y < b.y + b.height + 10. {
                    let mut x = b.x - 10.;
                    while x < b.x + b.width + 10. {
                        let p = Point { x, y };
                        if (near(p, &raw) - reach).abs() > 2. {
                            let exact = b.contains(p, reach) && near(p, &fitted) <= reach;
                            assert_eq!(pen.hit(p, tolerance, false), exact, "{p:?}");
                        }
                        x += 1.3;
                    }
                    y += 1.3;
                }
            }
        }
    }

    #[test]
    fn polygon_shapes_fill_their_drag_box_and_hit_by_outline_or_fill() {
        for tool in [Tool::Triangle, Tool::Diamond, Tool::Star] {
            let mut a = Annotation::new(tool, Point { x: 100., y: 50. }, Style::default());
            a.update(Point { x: 300., y: 250. });
            let b = a.vector_path().bounds().unwrap();
            assert!(
                (b.x - 100.).abs() < 0.01 && (b.y - 50.).abs() < 0.01,
                "{tool:?}"
            );
            assert!((b.width - 200.).abs() < 0.01, "{tool:?}");
            assert!((b.height - 200.).abs() < 0.01, "{tool:?}");
            let center = Point { x: 200., y: 160. };
            assert!(!a.hit(center, 2., false), "{tool:?} 空心中间不命中");
            assert!(a.hit(center, 2., true), "{tool:?} 编辑时内部可点选");
            a.style.filled = true;
            assert!(a.hit(center, 2., false), "{tool:?}");
            assert_eq!(a.vector_path().fills.len(), 1);
            // 外接框角落在形状外。
            assert!(!a.hit(Point { x: 104., y: 54. }, 2., true), "{tool:?}");
        }
        assert_eq!(
            polygon_corners(
                Tool::Star,
                Bounds {
                    x: 0.,
                    y: 0.,
                    width: 10.,
                    height: 10.
                }
            )
            .len(),
            10
        );
    }
}

#[cfg(test)]
mod effect_region_tests {
    use super::*;

    fn composite() -> Annotation {
        let mut a = Annotation::new(Tool::Mosaic, Point { x: 0., y: 0. }, Style::default());
        a.set_effect_regions(
            vec![
                Bounds {
                    x: 10.,
                    y: 20.,
                    width: 10.,
                    height: 10.,
                },
                Bounds {
                    x: 40.,
                    y: 20.,
                    width: 10.,
                    height: 10.,
                },
            ],
            true,
        )
        .unwrap();
        a
    }
    fn near(a: f32, b: f32) {
        assert!((a - b).abs() < 0.0001, "{a} != {b}");
    }

    #[test]
    fn sparse_effect_transforms_update_real_geometry_detach_management_and_preserve_gaps() {
        let original = composite();
        assert!(original.valid() && original.effect_regions_managed());
        assert!(original.hit(Point { x: 15., y: 25. }, 0., true));
        assert!(!original.hit(Point { x: 30., y: 25. }, 0., true));

        let mut moved = original.clone();
        moved.translate(12., -5.);
        assert_eq!(
            moved.effect_regions_world()[0],
            Bounds {
                x: 22.,
                y: 15.,
                width: 10.,
                height: 10.
            }
        );
        assert!(!moved.effect_regions_managed());
        assert!(original.effect_regions_managed());

        let resized = original.resized(Bounds {
            x: 0.,
            y: 0.,
            width: 80.,
            height: 20.,
        });
        assert_eq!(
            resized.effect_regions_world(),
            vec![
                Bounds {
                    x: 0.,
                    y: 0.,
                    width: 20.,
                    height: 20.
                },
                Bounds {
                    x: 60.,
                    y: 0.,
                    width: 20.,
                    height: 20.
                },
            ]
        );
        assert!(!resized.effect_regions_managed());
        assert!(!resized.hit(Point { x: 40., y: 10. }, 0., true));

        let mut scaled = original.clone();
        scaled.scale_about(Point { x: 10., y: 20. }, 2.);
        assert_eq!(
            scaled.effect_regions_world()[1],
            Bounds {
                x: 70.,
                y: 20.,
                width: 20.,
                height: 20.
            }
        );
        assert!(!scaled.effect_regions_managed());

        let mut rotated = original.clone();
        rotated.rotate_about(Point { x: 0., y: 0. }, 90.);
        assert!(rotated.valid() && !rotated.effect_regions_managed());
        let world = rotated.effect_regions_world()[0];
        near(world.x, -30.);
        near(world.y, 10.);
        near(world.width, 10.);
        near(world.height, 10.);
        assert!(rotated.hit(Point { x: -25., y: 15. }, 0.001, true));
        assert!(!rotated.hit(Point { x: -25., y: 30. }, 0., true));
        assert!(original.effect_regions_managed());
    }

    #[test]
    fn sparse_effect_setter_rejects_invalid_regions_without_mutation_and_smart_is_only_a_mode() {
        let mut a = composite();
        let original = a.clone();
        for regions in [
            vec![],
            vec![Bounds {
                x: f32::NAN,
                y: 0.,
                width: 1.,
                height: 1.,
            }],
            vec![Bounds {
                x: 0.,
                y: 0.,
                width: 0.,
                height: 1.,
            }],
            vec![Bounds {
                x: f32::MAX,
                y: 0.,
                width: f32::MAX,
                height: 1.,
            }],
        ] {
            assert!(a.set_effect_regions(regions, true).is_err());
            assert_eq!(a, original);
        }
        a.translate(0., 0.);
        a.scale_about(Point { x: 0., y: 0. }, 1.);
        a.rotate_about(Point { x: f32::NAN, y: 0. }, 30.);
        assert_eq!(a, original);
        assert_eq!(a.resized(a.local_bounds()), original);
        let mut uneven = Annotation::new(Tool::Mosaic, Point { x: 0., y: 0. }, Style::default());
        uneven
            .set_effect_regions(
                vec![
                    Bounds {
                        x: 0.,
                        y: 0.,
                        width: 6.,
                        height: 1.,
                    },
                    Bounds {
                        x: 7.,
                        y: 0.,
                        width: 6.,
                        height: 1.,
                    },
                ],
                true,
            )
            .unwrap();
        assert_eq!(uneven.local_bounds().width, 13.);
        assert_eq!(uneven.resized(uneven.local_bounds()), uneven);
        let mut fractional = Annotation::new(Tool::Blur, Point { x: 0., y: 0. }, Style::default());
        fractional
            .set_effect_regions(
                vec![Bounds {
                    x: 0.1,
                    y: 0.2,
                    width: 0.3,
                    height: 0.4,
                }],
                true,
            )
            .unwrap();
        let fractional_before = fractional.clone();
        fractional.scale_about(
            Point {
                x: 5000.,
                y: -5000.,
            },
            1.,
        );
        assert_eq!(fractional, fractional_before);
        assert_eq!(
            fractional.effect_regions_world(),
            fractional_before.effect_regions_world()
        );
        assert!(!Tool::SmartMosaic.draws());
        assert!(!Tool::SmartMosaic.effect());
        let mut mode = Annotation::new(Tool::SmartMosaic, Point { x: 0., y: 0. }, Style::default());
        assert!(mode
            .set_effect_regions(original.effect_regions_world(), true)
            .is_err());
        assert!(!mode.valid());
        let mut invalid = original;
        invalid.rotation = f32::NAN;
        assert!(!invalid.valid() && !invalid.effect_regions_managed());
    }
}
