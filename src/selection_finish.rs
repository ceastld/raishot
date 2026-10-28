//! 选区外观的纯模型与 BGRA 合成。长度均为物理像素，颜色为 0xAARRGGBB。
//!
//! 与截图标注、GPU 和窗口生命周期无关：调用者先完成裁剪/套索/标注，再传入图片。
//! 预览缓存应以选区尺寸和 normalized style 为键；不要随鼠标悬停重复生成位图。
use crate::{capture::Image, geometry::Rect, Result};

pub(crate) const DEFAULT_CORNER_RADIUS: f32 = 12.;
pub(crate) const MAX_CORNER_RADIUS: f32 = 60.;
pub(crate) const MAX_SHADOW_BLUR: f32 = 48.;
pub(crate) const MAX_BORDER_THICKNESS: f32 = 8.;
// 至多保留输入和一张输出，不分配整图浮点/alpha mask；两者合计不超过 512 MiB。
const MAX_IMAGE_BYTES: usize = 256 * 1024 * 1024;
const HALF_PIXEL_DIAGONAL: f64 = std::f64::consts::FRAC_1_SQRT_2;

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SelectionFinishStyle {
    pub corner_enabled: bool,
    /// 关闭圆角仍保留最后档位；有效半径还会受选区短边限制。
    pub corner_radius: f32,
    pub shadow_enabled: bool,
    pub shadow_color: u32,
    pub shadow_blur: f32,
    pub shadow_opacity: f32,
    pub border_enabled: bool,
    pub border_color: u32,
    pub border_thickness: f32,
    pub border_inside: bool,
    pub remember_enabled: bool,
}
impl Default for SelectionFinishStyle {
    fn default() -> Self {
        Self {
            corner_enabled: false,
            corner_radius: DEFAULT_CORNER_RADIUS,
            shadow_enabled: false,
            shadow_color: 0xff00_0000,
            shadow_blur: 10.,
            shadow_opacity: 0.36,
            border_enabled: false,
            border_color: 0xffff_ffff,
            border_thickness: 2.,
            border_inside: false,
            remember_enabled: false,
        }
    }
}

/// 选区外观数值入口的显示单位；透明度是百分数，长度是原图物理像素。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinishField {
    Radius,
    Blur,
    Opacity,
    Thickness,
}

impl FinishField {
    pub(crate) fn limits(self) -> (f32, f32) {
        match self {
            Self::Radius => (0., MAX_CORNER_RADIUS),
            Self::Blur => (0., MAX_SHADOW_BLUR),
            Self::Opacity => (0., 100.),
            // 0 即关闭边框；开启后的最小有效宽度仍由 `normalized` 保证为 1px。
            Self::Thickness => (0., MAX_BORDER_THICKNESS),
        }
    }

    /// 显示值：圆角、扩散、粗细在对应效果关闭时为 0（0 即关），关闭不清掉最后档位。
    pub(crate) fn value(self, style: SelectionFinishStyle) -> f32 {
        match self {
            Self::Radius if style.corner_enabled => style.corner_radius,
            Self::Blur if style.shadow_enabled => style.shadow_blur,
            Self::Thickness if style.border_enabled => style.border_thickness,
            Self::Radius | Self::Blur | Self::Thickness => 0.,
            // 透明度按 0.1% 显示与比较；直接乘 100 会把 0.36 变成 36.000004。
            Self::Opacity => (style.shadow_opacity * 1000.).round() / 10.,
        }
    }

    pub(crate) fn applied(
        self,
        mut style: SelectionFinishStyle,
        value: f32,
    ) -> SelectionFinishStyle {
        // 同值确认不打开原本关闭的效果，也不把原有透明度经过百分数回算。
        if !value.is_finite() || value == self.value(style) {
            return style;
        }
        let (min, max) = self.limits();
        let value = value.clamp(min, max);
        // 圆角、扩散、粗细：0 是关闭命令，不消费最后档位。
        match self {
            Self::Radius => {
                if value > 0. {
                    style.corner_radius = value;
                }
                style.corner_enabled = value > 0.;
            }
            Self::Blur => {
                if value > 0. {
                    style.shadow_blur = value;
                    // 浓度曾被拖到 0 时恢复默认浓度，开启后必须可见。
                    return style.with_shadow_enabled(true);
                }
                style.shadow_enabled = false;
            }
            Self::Opacity => {
                style.shadow_opacity = value / 100.;
                style.shadow_enabled = value > 0.;
            }
            Self::Thickness => {
                if value > 0. {
                    style.border_thickness = value;
                }
                style.border_enabled = value > 0.;
            }
        }
        style.normalized()
    }
}

/// 快速外观模式共用一个强度；旧独立字段与组合样式仍保留原来的存储合同。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FinishEffect {
    #[default]
    Shadow,
    Border,
}

impl FinishEffect {
    pub(crate) fn intensity(self, style: SelectionFinishStyle) -> f32 {
        let value = match self {
            Self::Shadow => (style.shadow_blur - 4.) / 24. * 100.,
            Self::Border => (style.border_thickness - 1.) / 7. * 100.,
        };
        finite_clamp(value, 0., 100.)
    }

    pub(crate) fn with_intensity(
        self,
        mut style: SelectionFinishStyle,
        value: f32,
    ) -> SelectionFinishStyle {
        if !value.is_finite() {
            return style;
        }
        let t = value.clamp(0., 100.) / 100.;
        match self {
            Self::Shadow => {
                style.shadow_enabled = true;
                style.border_enabled = false;
                style.shadow_blur = 4. + 24. * t;
                style.shadow_opacity = 0.2 + 0.5 * t;
            }
            Self::Border => {
                style.border_enabled = true;
                style.shadow_enabled = false;
                style.border_thickness = 1. + 7. * t;
            }
        }
        style
    }
}

/// 选区外观样式：选区条上直接点选的五个槽位，各自记住一份完整外观（`FinishLooks`）。
/// 样式就是快照：缩略图、预览与导出一致，没有隐藏的记忆开关。
/// 旧配置只有 raw/card/stroke 三种“样式 + 记忆开关”，加载时按旧语义 `resolve` 一次。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FinishPreset {
    /// 不加任何效果。
    #[default]
    Raw,
    Round,
    Card,
    Stroke,
    /// 圆角 + 细边 + 阴影。
    Window,
}

impl FinishPreset {
    pub(crate) const ALL: [Self; 5] = [
        Self::Raw,
        Self::Round,
        Self::Card,
        Self::Stroke,
        Self::Window,
    ];

    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Round => "round",
            Self::Card => "card",
            Self::Stroke => "stroke",
            Self::Window => "window",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Raw => tr!("原样", "Plain"),
            Self::Round => tr!("圆角", "Rounded"),
            Self::Card => tr!("卡片", "Card"),
            Self::Stroke => tr!("描边", "Outline"),
            Self::Window => tr!("窗口", "Window"),
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.key() == value)
    }

    fn index(self) -> usize {
        self as usize
    }

    /// 每个样式的出厂外观；“恢复默认”回到这里。
    pub(crate) fn defaults(self) -> SelectionFinishStyle {
        let base = SelectionFinishStyle::default();
        match self {
            Self::Raw => base,
            Self::Round => SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 12.,
                ..base
            },
            Self::Card => SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 12.,
                shadow_enabled: true,
                shadow_blur: 16.,
                shadow_opacity: 0.42,
                ..base
            },
            Self::Stroke => SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 8.,
                border_enabled: true,
                border_thickness: 3.,
                ..base
            },
            Self::Window => SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 10.,
                shadow_enabled: true,
                shadow_blur: 20.,
                shadow_opacity: 0.45,
                border_enabled: true,
                border_color: 0xff9a_a5b1,
                border_thickness: 1.,
                ..base
            },
        }
    }

    /// 旧配置没有样式字段：按当时会恢复的开关推断，避免升级后外观突变。
    pub(crate) fn from_legacy(style: SelectionFinishStyle) -> Self {
        if style.remember_enabled {
            Self::matching(style)
        } else {
            Self::Raw
        }
    }

    /// 由开关归类：旧配置迁移、以及原样上调出效果时决定切到哪个样式。
    pub(crate) fn matching(style: SelectionFinishStyle) -> Self {
        match (style.shadow_enabled, style.border_enabled) {
            (true, true) => Self::Window,
            (true, false) => Self::Card,
            (false, true) => Self::Stroke,
            (false, false) if style.corner_enabled => Self::Round,
            (false, false) => Self::Raw,
        }
    }

    /// 调整后的外观归属：原样上调出任何效果（如拖圆角柄）就切到相符的样式，
    /// 其它样式只是被改了参数，仍是原来的样式。
    pub(crate) fn settle(self, style: SelectionFinishStyle) -> Self {
        if self == Self::Raw {
            Self::matching(style)
        } else {
            self
        }
    }

    /// 旧配置的“样式 + 记忆开关” → 实际外观；新样式本身就是快照，原样返回。
    pub(crate) fn resolve(self, memory: SelectionFinishStyle) -> SelectionFinishStyle {
        let mut style = memory.normalized();
        match self {
            Self::Raw => {
                style.shadow_enabled = false;
                style.border_enabled = false;
            }
            Self::Card => {
                style = style.with_shadow_enabled(true);
                style.border_enabled = false;
            }
            Self::Stroke => style.border_enabled = true,
            Self::Round | Self::Window => {}
        }
        style.normalized()
    }
}

/// 五个样式各自记住的外观。原样没有参数，始终是全关；`remember_enabled` 不进槽位。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FinishLooks([SelectionFinishStyle; 5]);

impl Default for FinishLooks {
    fn default() -> Self {
        Self(FinishPreset::ALL.map(FinishPreset::defaults))
    }
}

impl FinishLooks {
    pub(crate) fn get(&self, preset: FinishPreset) -> SelectionFinishStyle {
        self.0[preset.index()]
    }

    pub(crate) fn set(&mut self, preset: FinishPreset, style: SelectionFinishStyle) {
        if preset != FinishPreset::Raw {
            self.0[preset.index()] = SelectionFinishStyle {
                remember_enabled: false,
                ..style.normalized()
            };
        }
    }
}

// 面板已改为直接调扩散/浓度；三档预设只留给模型测试。
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShadowPreset {
    Soft,
    Standard,
    Strong,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FinishPadding {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}
impl FinishPadding {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
    pub fn horizontal(self) -> u32 {
        self.left + self.right
    }
    pub fn vertical(self) -> u32 {
        self.top + self.bottom
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FinishLayout {
    pub output_width: u32,
    pub output_height: u32,
    /// 内容在扩展后图片中的位置；原有全屏选区坐标不随装饰改变。
    pub content_rect: Rect,
    pub padding: FinishPadding,
    pub geometry: RoundedGeometry,
}

/// 公共的圆角轮廓核，供 GPUI 遮罩与导出共同使用；采样坐标相对内容左上角。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RoundedGeometry {
    pub width: u32,
    pub height: u32,
    pub radius: f64,
}
impl RoundedGeometry {
    pub(crate) fn border_band(self, inside: bool, thickness: f64) -> (f64, f64) {
        if inside {
            // 内轮廓耗尽后没有孔，不能在最深内距处留下半透明中缝。
            let inner = if thickness >= f64::from(self.width.min(self.height)) * 0.5 {
                f64::NEG_INFINITY
            } else {
                -thickness
            };
            (inner, 0.)
        } else {
            (0., thickness)
        }
    }
    /// 到圆角矩形的有符号距离，内部为负，外部为正。
    pub fn signed_distance(self, x: f64, y: f64) -> f64 {
        if !x.is_finite() || !y.is_finite() {
            return f64::INFINITY;
        }
        let half_w = self.width as f64 * 0.5;
        let half_h = self.height as f64 * 0.5;
        let px = (x - half_w).abs() - half_w + self.radius;
        let py = (y - half_h).abs() - half_h + self.radius;
        px.max(0.).hypot(py.max(0.)) + px.max(py).min(0.) - self.radius
    }
    /// 直角内容的边框也保持直角；阴影仍沿欧氏距离自然扩散。
    pub(crate) fn border_distance(self, x: f64, y: f64) -> f64 {
        if self.radius > 0. {
            return self.signed_distance(x, y);
        }
        if !x.is_finite() || !y.is_finite() {
            return f64::INFINITY;
        }
        let half_w = self.width as f64 * 0.5;
        let half_h = self.height as f64 * 0.5;
        ((x - half_w).abs() - half_w).max((y - half_h).abs() - half_h)
    }
    /// 边框在目标像素矩形内的面积覆盖率；像素尺寸以内容坐标计。
    pub(crate) fn border_coverage_at(
        self,
        x: f64,
        y: f64,
        inside: bool,
        thickness: f64,
        pixel_width: f64,
        pixel_height: f64,
    ) -> f64 {
        if !x.is_finite()
            || !y.is_finite()
            || !thickness.is_finite()
            || thickness <= 0.
            || !pixel_width.is_finite()
            || !pixel_height.is_finite()
            || pixel_width <= 0.
            || pixel_height <= 0.
        {
            return 0.;
        }
        let (inner, outer) = self.border_band(inside, thickness);
        let sd = self.border_distance(x, y);
        let reach = (pixel_width * 0.5).hypot(pixel_height * 0.5);
        if sd <= inner - reach || sd >= outer + reach {
            return 0.;
        }
        if sd >= inner + reach && sd <= outer - reach {
            return 1.;
        }
        let outer_coverage = self.contour_coverage_at(x, y, outer, pixel_width, pixel_height);
        let inner_coverage = if inner.is_finite() {
            self.contour_coverage_at(x, y, inner, pixel_width, pixel_height)
        } else {
            0.
        };
        (outer_coverage - inner_coverage).clamp(0., 1.)
    }

    fn contour_coverage_at(
        self,
        x: f64,
        y: f64,
        outward: f64,
        pixel_width: f64,
        pixel_height: f64,
    ) -> f64 {
        let (left, top) = (-outward, -outward);
        let (right, bottom) = (self.width as f64 + outward, self.height as f64 + outward);
        if left >= right || top >= bottom {
            return 0.;
        }
        let radius = if self.radius > 0. {
            (self.radius + outward)
                .max(0.)
                .min((right - left).min(bottom - top) * 0.5)
        } else {
            0.
        };
        let x0 = (x - pixel_width * 0.5).max(left);
        let x1 = (x + pixel_width * 0.5).min(right);
        let y0 = (y - pixel_height * 0.5).max(top);
        let y1 = (y + pixel_height * 0.5).min(bottom);
        if x0 >= x1 || y0 >= y1 {
            return 0.;
        }
        let pixel_area = pixel_width * pixel_height;
        // 直边精确求矩形交面积，分数缩放不受子像素采样档位影响。
        if radius == 0.
            || (x0 >= left + radius && x1 <= right - radius)
            || (y0 >= top + radius && y1 <= bottom - radius)
        {
            return (x1 - x0) * (y1 - y0) / pixel_area;
        }
        // 只在曲线相交像素沿 y 积分；每条横截面与圆弧的交宽解析计算。
        let step = (y1 - y0) / 32.;
        let mut width_sum = 0.;
        for row in 0..32 {
            let sample_y = y0 + (row as f64 + 0.5) * step;
            let dy = (top + radius - sample_y)
                .max(sample_y - (bottom - radius))
                .max(0.);
            let inset = radius - (radius * radius - dy * dy).max(0.).sqrt();
            width_sum += (x1.min(right - inset) - x0.max(left + inset)).max(0.);
        }
        (width_sum * step / pixel_area).clamp(0., 1.)
    }
    /// 像素中心的面积覆盖率。仅轮廓相交像素进行 8×8 采样，没有全图 mask。
    pub fn coverage_at(self, x: f64, y: f64) -> f64 {
        let sd = self.signed_distance(x, y);
        if sd <= -HALF_PIXEL_DIAGONAL {
            return 1.;
        }
        if sd >= HALF_PIXEL_DIAGONAL {
            return 0.;
        }
        let mut covered = 0_u32;
        for sy in 0..8 {
            let y = y - 0.5 + (sy as f64 + 0.5) / 8.;
            for sx in 0..8 {
                let x = x - 0.5 + (sx as f64 + 0.5) / 8.;
                covered += u32::from(self.contains(x, y));
            }
        }
        covered as f64 / 64.
    }
    pub fn contains(self, x: f64, y: f64) -> bool {
        let w = self.width as f64;
        let h = self.height as f64;
        if !(0. <= x && x <= w && 0. <= y && y <= h) {
            return false;
        }
        let r = self.radius;
        if r <= 0. || (r <= x && x <= w - r) || (r <= y && y <= h - r) {
            return true;
        }
        let cx = if x < r { r } else { w - r };
        let cy = if y < r { r } else { h - r };
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r
    }
}

impl SelectionFinishStyle {
    pub(crate) fn exclusive_effect(self) -> Option<FinishEffect> {
        match (self.shadow_enabled, self.border_enabled) {
            (true, false) => Some(FinishEffect::Shadow),
            (false, true) => Some(FinishEffect::Border),
            _ => None,
        }
    }

    /// 数值 0 关闭阴影后，再开应恢复可见档位，并保留独立边框及其它参数。
    pub(crate) fn with_shadow_enabled(self, enabled: bool) -> Self {
        let mut style = self.normalized();
        if enabled {
            let defaults = Self::default();
            if style.shadow_blur <= 0. {
                style.shadow_blur = defaults.shadow_blur;
            }
            if style.shadow_opacity <= 0. {
                style.shadow_opacity = defaults.shadow_opacity;
            }
        }
        style.shadow_enabled = enabled;
        style
    }

    pub fn normalized(mut self) -> Self {
        self.corner_radius = finite_clamp(self.corner_radius, 0., MAX_CORNER_RADIUS);
        if self.corner_enabled && self.corner_radius <= 0. {
            self.corner_radius = DEFAULT_CORNER_RADIUS;
        }
        self.shadow_blur = finite_clamp(self.shadow_blur, 0., MAX_SHADOW_BLUR);
        self.shadow_opacity = finite_clamp(self.shadow_opacity, 0., 1.);
        self.border_thickness = finite_clamp(self.border_thickness, 0., MAX_BORDER_THICKNESS);
        if self.border_enabled && self.border_thickness < 1. {
            self.border_thickness = 1.;
        }
        self
    }
    /// 样式与圆角/阴影记忆开关始终跨截图保留；是否生效由 `FinishPreset` 决定。
    pub fn prepare_for_new_capture(&mut self) {
        *self = self.normalized();
    }
    #[cfg(test)]
    pub fn apply_shadow_preset(&mut self, preset: ShadowPreset) {
        (self.shadow_blur, self.shadow_opacity) = match preset {
            ShadowPreset::Soft => (10., 0.36),
            ShadowPreset::Standard => (16., 0.45),
            ShadowPreset::Strong => (28., 0.55),
        };
    }
    pub fn has_effect(self) -> bool {
        let s = self.normalized();
        s.corner_enabled || s.has_shadow() || s.has_border()
    }
    pub fn padding(self) -> FinishPadding {
        let s = self.normalized();
        let shadow = if s.has_shadow() {
            s.shadow_reach().ceil() as u32
        } else {
            0
        };
        let border = if s.has_border() && !s.border_inside {
            s.border_thickness.ceil() as u32
        } else {
            0
        };
        let pad = shadow.max(border);
        FinishPadding {
            left: pad,
            top: pad,
            right: pad,
            bottom: pad,
        }
    }
    pub fn geometry(self, width: u32, height: u32) -> RoundedGeometry {
        let s = self.normalized();
        RoundedGeometry {
            width,
            height,
            radius: if s.corner_enabled {
                (s.corner_radius as f64).min(width.min(height) as f64 * 0.5)
            } else {
                0.
            },
        }
    }
    pub fn layout(self, width: u32, height: u32) -> Result<FinishLayout> {
        image_bytes(width, height)?;
        let padding = self.padding();
        let output_width = width
            .checked_add(padding.horizontal())
            .ok_or("选区外观输出宽度溢出")?;
        let output_height = height
            .checked_add(padding.vertical())
            .ok_or("选区外观输出高度溢出")?;
        image_bytes(output_width, output_height)?;
        Ok(FinishLayout {
            output_width,
            output_height,
            content_rect: Rect {
                x: padding.left,
                y: padding.top,
                width,
                height,
            },
            padding,
            geometry: self.geometry(width, height),
        })
    }
    /// 消费输入，以免无扩展时仍克隆整图。输出仍为 straight BGRA，不预乘 RGB。
    pub fn compose(self, mut source: Image) -> Result<Image> {
        let expected = image_bytes(source.width, source.height)?;
        if source.bgra.len() != expected {
            return Err("选区外观输入的 BGRA 长度与尺寸不一致".into());
        }
        let s = self.normalized();
        let layout = s.layout(source.width, source.height)?;
        let region = Rect::full(layout.output_width, layout.output_height);
        clip_corners(&mut source, layout.geometry);
        if layout.padding.is_empty() {
            if s.has_border() {
                paint_border(&mut source, layout, region, s);
            }
            return Ok(source);
        }
        let mut output = empty_image(
            layout.output_width,
            layout.output_height,
            source.readback_ms,
        )?;
        if s.has_shadow() {
            paint_shadow(&mut output, layout, region, s);
        }
        blit_source(&mut output, &source, layout, s.has_shadow());
        // 原图已不再需要，在边框阶段前释放输入，限制峰值工作集。
        drop(source);
        if s.has_border() {
            paint_border(&mut output, layout, region, s);
        }
        Ok(output)
    }
    /// 输出坐标中的局部阴影图。预览只缓存四条边带，无需整幅透明 overlay。
    pub fn shadow_region(self, width: u32, height: u32, region: Rect) -> Result<Image> {
        self.render_region(width, height, region, true, false)
    }
    /// 输出坐标中的局部边框图；返回尺寸就是 region.width/height。
    pub fn border_region(self, width: u32, height: u32, region: Rect) -> Result<Image> {
        self.render_region(width, height, region, false, true)
    }
    /// 装饰合成图。半透明边缘要求严格层序时使用下面两个分层 API。
    #[cfg(test)]
    pub fn overlay(self, width: u32, height: u32) -> Result<Image> {
        self.render_overlay(width, height, true, true)
    }
    /// 阴影应位于圆角裁剪后的内容下方，不填充套索/PNG 的透明内部。
    #[cfg(test)]
    pub fn shadow_overlay(self, width: u32, height: u32) -> Result<Image> {
        self.render_overlay(width, height, true, false)
    }
    /// 边框应位于内容上方，与导出共用距离带和 alpha 合成。
    #[cfg(test)]
    pub fn border_overlay(self, width: u32, height: u32) -> Result<Image> {
        self.render_overlay(width, height, false, true)
    }
    #[cfg(test)]
    fn render_overlay(self, width: u32, height: u32, shadow: bool, border: bool) -> Result<Image> {
        let layout = self.layout(width, height)?;
        self.render_region(
            width,
            height,
            Rect::full(layout.output_width, layout.output_height),
            shadow,
            border,
        )
    }
    fn render_region(
        self,
        width: u32,
        height: u32,
        region: Rect,
        shadow: bool,
        border: bool,
    ) -> Result<Image> {
        let s = self.normalized();
        let layout = s.layout(width, height)?;
        if !region.valid(layout.output_width, layout.output_height) {
            return Err("选区外观预览区域为空或超出输出范围".into());
        }
        let mut image = empty_image(region.width, region.height, 0.)?;
        if shadow && s.has_shadow() {
            paint_shadow(&mut image, layout, region, s);
        }
        if border && s.has_border() {
            paint_border(&mut image, layout, region, s);
        }
        Ok(image)
    }
    /// 阴影从内容外沿外推多少才开始衰减：外侧边框时为边框粗细，阴影从边框外沿投出，
    /// 不被边框盖住；内侧边框或无边框时为 0。
    pub(crate) fn shadow_offset(self) -> f64 {
        let s = self.normalized();
        if s.has_border() && !s.border_inside {
            s.border_thickness as f64
        } else {
            0.
        }
    }
    /// 阴影最远到达的距内容外沿距离。
    pub(crate) fn shadow_reach(self) -> f64 {
        self.shadow_offset() + self.normalized().shadow_blur as f64
    }
    /// 距内容外沿 `sd`（外部为正）处的阴影强度 0..1，未乘浓度。边框下方按峰值铺满，
    /// 避免边框外沿抗锯齿处露出底色缝；不透明边框会盖住这一段。
    pub(crate) fn shadow_strength(self, sd: f64) -> f64 {
        let blur = self.normalized().shadow_blur as f64;
        if sd <= 0. || blur <= 0. || sd >= self.shadow_reach() {
            return 0.;
        }
        shadow_falloff((sd - self.shadow_offset()).max(0.) / blur)
    }
    pub(crate) fn has_shadow(self) -> bool {
        self.shadow_enabled
            && self.shadow_blur > 0.
            && self.shadow_opacity > 0.
            && self.shadow_color >> 24 > 0
    }
    pub(crate) fn has_border(self) -> bool {
        self.border_enabled && self.border_thickness > 0. && self.border_color >> 24 > 0
    }
}

fn finite_clamp(value: f32, min: f32, max: f32) -> f32 {
    if value.is_nan() {
        min
    } else {
        value.clamp(min, max)
    }
}
fn image_bytes(width: u32, height: u32) -> Result<usize> {
    let len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or("选区外观图片尺寸溢出")?;
    if width == 0 || height == 0 || len > MAX_IMAGE_BYTES {
        return Err("选区外观图片为空或超过 256 MiB 输出上限".into());
    }
    Ok(len)
}
fn empty_image(width: u32, height: u32, readback_ms: f64) -> Result<Image> {
    let len = image_bytes(width, height)?;
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(len)?;
    bgra.resize(len, 0);
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms,
    })
}
fn color_bgra(argb: u32) -> [u8; 4] {
    [
        argb as u8,
        (argb >> 8) as u8,
        (argb >> 16) as u8,
        (argb >> 24) as u8,
    ]
}
fn clip_corners(image: &mut Image, geometry: RoundedGeometry) {
    let span = geometry.radius.ceil() as u32;
    // 半径最多 60px，只触及四个角；奇数短边时避免重复衰减中间那行/列。
    for y in 0..span {
        for x in 0..span {
            let coverage = geometry.coverage_at(x as f64 + 0.5, y as f64 + 0.5);
            if coverage >= 1. {
                continue;
            }
            for (iy, dy) in [y, image.height - 1 - y].into_iter().enumerate() {
                if iy > 0 && dy == y {
                    continue;
                }
                for (ix, dx) in [x, image.width - 1 - x].into_iter().enumerate() {
                    if ix > 0 && dx == x {
                        continue;
                    }
                    let i = (dy as usize * image.width as usize + dx as usize) * 4;
                    let alpha = (image.bgra[i + 3] as f64 * coverage).round() as u8;
                    if alpha == 0 {
                        image.bgra[i..i + 4].fill(0);
                    } else {
                        image.bgra[i + 3] = alpha;
                    }
                }
            }
        }
    }
}
fn blit_source(dst: &mut Image, source: &Image, layout: FinishLayout, shadow: bool) {
    let w = source.width as usize;
    let stride = w * 4;
    let dst_stride = dst.width as usize * 4;
    let corner = layout.geometry.radius.ceil() as usize;
    for y in 0..source.height as usize {
        let src = &source.bgra[y * stride..(y + 1) * stride];
        let start =
            (y + layout.padding.top as usize) * dst_stride + layout.padding.left as usize * 4;
        let row = &mut dst.bgra[start..start + stride];
        if !shadow
            || (y as f64 >= layout.geometry.radius
                && y as f64 + 1. <= source.height as f64 - layout.geometry.radius)
        {
            row.copy_from_slice(src);
            continue;
        }
        let left = corner.min(w);
        let right = w.saturating_sub(corner).max(left).min(w);
        row[left * 4..right * 4].copy_from_slice(&src[left * 4..right * 4]);
        for x in (0..left).chain(right..w) {
            blend_over(
                &mut row[x * 4..x * 4 + 4],
                src[x * 4..x * 4 + 4].try_into().unwrap(),
            );
        }
    }
}
/// 只遍历轮廓边缘带。长截图的中间大块通过行切片复制，不逐像素算 sqrt。
fn boundary_pixels(
    layout: FinishLayout,
    region: Rect,
    band: f64,
    distance: fn(RoundedGeometry, f64, f64) -> f64,
    mut paint: impl FnMut(usize, f64),
) {
    let rect = layout.content_rect;
    let span = band.ceil() as u32;
    let corner_band = layout.geometry.radius + band;
    let left_end = rect.x.saturating_add(span).min(layout.output_width);
    let right_start = (rect.x + rect.width)
        .saturating_sub(span)
        .min(layout.output_width);
    let region_right = region.x + region.width;
    for y in region.y..region.y + region.height {
        let local_y = y as f64 + 0.5 - rect.y as f64;
        let full_row = local_y <= corner_band
            || local_y >= rect.height as f64 - corner_band
            || left_end >= right_start;
        let mut visit = |x: u32| {
            let sd = distance(layout.geometry, x as f64 + 0.5 - rect.x as f64, local_y);
            paint(
                ((y - region.y) as usize * region.width as usize + (x - region.x) as usize) * 4,
                sd,
            );
        };
        if full_row {
            for x in region.x..region_right {
                visit(x);
            }
        } else {
            for x in (region.x..left_end.min(region_right))
                .chain(right_start.max(region.x)..region_right)
            {
                visit(x);
            }
        }
    }
}
fn shadow_falloff(t: f64) -> f64 {
    let t = t.clamp(0., 1.);
    (1. - t).powi(3) * (1. + 2. * t)
}
fn paint_shadow(
    image: &mut Image,
    layout: FinishLayout,
    region: Rect,
    style: SelectionFinishStyle,
) {
    let color = color_bgra(style.shadow_color);
    let peak = style.shadow_opacity as f64 * color[3] as f64;
    boundary_pixels(
        layout,
        region,
        style.shadow_reach(),
        RoundedGeometry::signed_distance,
        |i, sd| {
            // 不铺整个不透明底形：套索透明处和半透明 PNG 内部必须保持透明。
            let strength = style.shadow_strength(sd);
            if strength <= 0. {
                return;
            }
            let alpha = (peak * strength).round() as u8;
            if alpha != 0 {
                image.bgra[i..i + 4].copy_from_slice(&[color[0], color[1], color[2], alpha]);
            }
        },
    );
}
fn paint_border(
    image: &mut Image,
    layout: FinishLayout,
    region: Rect,
    style: SelectionFinishStyle,
) {
    let color = color_bgra(style.border_color);
    let thickness = style.border_thickness as f64;
    boundary_pixels(
        layout,
        region,
        thickness + 1.25,
        RoundedGeometry::border_distance,
        |i, _| {
            let pixel = (i / 4) as u32;
            let x =
                f64::from(region.x + pixel % region.width) - f64::from(layout.content_rect.x) + 0.5;
            let y =
                f64::from(region.y + pixel / region.width) - f64::from(layout.content_rect.y) + 0.5;
            let coverage =
                layout
                    .geometry
                    .border_coverage_at(x, y, style.border_inside, thickness, 1., 1.);
            if coverage <= 0. {
                return;
            }
            let alpha = (color[3] as f64 * coverage).round() as u8;
            blend_over(
                &mut image.bgra[i..i + 4],
                [color[0], color[1], color[2], alpha],
            );
        },
    );
}
/// 精确计算 straight-alpha source-over，最后再量化。透明 RGB 不参与混合。
fn blend_over(dst: &mut [u8], src: [u8; 4]) {
    let sa = src[3] as u32;
    if sa == 0 {
        return;
    }
    let da = dst[3] as u32;
    if sa == 255 || da == 0 {
        dst.copy_from_slice(&src);
        return;
    }
    let inverse = 255 - sa;
    let alpha_numerator = sa * 255 + da * inverse;
    for channel in 0..3 {
        let numerator = src[channel] as u32 * sa * 255 + dst[channel] as u32 * da * inverse;
        dst[channel] = ((numerator + alpha_numerator / 2) / alpha_numerator) as u8;
    }
    dst[3] = ((alpha_numerator + 127) / 255) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_finish_shadow_reenable_after_zero_keeps_border_and_restores_visible_output() {
        for field in [FinishField::Blur, FinishField::Opacity] {
            let original = SelectionFinishStyle {
                shadow_enabled: true,
                border_enabled: true,
                border_color: 0xff12_3456,
                border_thickness: 3.,
                ..Default::default()
            };
            let off = field.applied(original, 0.);
            assert!(!off.has_shadow());
            let restored = off.with_shadow_enabled(true);
            assert!(restored.has_shadow());
            assert!(restored.border_enabled);
            assert_eq!(restored.border_color, original.border_color);
            assert_eq!(restored.border_thickness, original.border_thickness);
            let output = restored.compose(image(16, 12, [255; 4])).unwrap();
            assert!(output.width > 16 && output.height > 12);
            assert!(output
                .bgra
                .chunks_exact(4)
                .any(|pixel| pixel[3] > 0 && pixel[3] < 255));
        }
    }

    fn image(width: u32, height: u32, color: [u8; 4]) -> Image {
        Image {
            width,
            height,
            bgra: color.repeat(width as usize * height as usize),
            readback_ms: 17.5,
        }
    }
    fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let i = (y as usize * image.width as usize + x as usize) * 4;
        image.bgra[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn selection_effect_intensity_matches_reference_golden_mapping() {
        assert_eq!(FinishEffect::default(), FinishEffect::Shadow);
        let initial = SelectionFinishStyle::default();
        for (intensity, blur, opacity, thickness) in [
            (0., 4., 0.2, 1.),
            (25., 10., 0.325, 2.75),
            (50., 16., 0.45, 4.5),
            (75., 22., 0.575, 6.25),
            (100., 28., 0.7, 8.),
        ] {
            let shadow = FinishEffect::Shadow.with_intensity(initial, intensity);
            assert_eq!(shadow.exclusive_effect(), Some(FinishEffect::Shadow));
            assert_eq!(shadow.shadow_blur, blur);
            assert!((shadow.shadow_opacity - opacity).abs() < 0.000001);
            assert!((FinishEffect::Shadow.intensity(shadow) - intensity).abs() < 0.0001);
            assert!(shadow.has_shadow());
            let border = FinishEffect::Border.with_intensity(initial, intensity);
            assert_eq!(border.exclusive_effect(), Some(FinishEffect::Border));
            assert_eq!(border.border_thickness, thickness);
            assert!((FinishEffect::Border.intensity(border) - intensity).abs() < 0.0001);
            assert!(border.has_border());
        }
    }

    #[test]
    fn selection_effect_bounds_and_nonfinite_input_keep_existing_parameters() {
        let style = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 17.25,
            shadow_enabled: true,
            border_enabled: true,
            remember_enabled: true,
            ..Default::default()
        };
        for effect in [FinishEffect::Shadow, FinishEffect::Border] {
            assert_eq!(
                effect.with_intensity(style, -100.),
                effect.with_intensity(style, 0.)
            );
            assert_eq!(
                effect.with_intensity(style, 200.),
                effect.with_intensity(style, 100.)
            );
            for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                assert_eq!(effect.with_intensity(style, value), style);
            }
        }
        for (value, expected) in [
            (f32::NAN, 0.),
            (f32::NEG_INFINITY, 0.),
            (f32::INFINITY, 100.),
        ] {
            assert_eq!(
                FinishEffect::Shadow.intensity(SelectionFinishStyle {
                    shadow_blur: value,
                    ..style
                }),
                expected
            );
            assert_eq!(
                FinishEffect::Border.intensity(SelectionFinishStyle {
                    border_thickness: value,
                    ..style
                }),
                expected
            );
        }
    }

    #[test]
    fn selection_effect_linked_edits_preserve_metadata_and_independent_field_contract() {
        let original = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 17.25,
            shadow_enabled: true,
            shadow_color: 0x80332211,
            shadow_blur: 37.25,
            shadow_opacity: 0.71,
            border_enabled: true,
            border_color: 0xaabbccdd,
            border_thickness: 6.125,
            border_inside: true,
            remember_enabled: true,
        };
        assert_eq!(
            FinishEffect::Shadow.with_intensity(original, 25.),
            SelectionFinishStyle {
                shadow_blur: 10.,
                shadow_opacity: 0.325,
                border_enabled: false,
                ..original
            }
        );
        assert_eq!(
            FinishEffect::Border.with_intensity(original, 25.),
            SelectionFinishStyle {
                border_thickness: 2.75,
                shadow_enabled: false,
                ..original
            }
        );
        // 高级独立参数仍允许旧组合；快速模式不改变这条兼容合同。
        let independent = FinishField::Blur.applied(original, 20.);
        assert!(independent.shadow_enabled && independent.border_enabled);
        assert_eq!(independent.shadow_opacity, original.shadow_opacity);
        assert_eq!(independent.border_thickness, original.border_thickness);
    }

    #[test]
    fn selection_effect_mapping_controls_padding_and_preserves_output_content_origin() {
        for (effect, intensity, padding) in [
            (FinishEffect::Shadow, 0., 4),
            (FinishEffect::Shadow, 50., 16),
            (FinishEffect::Border, 0., 1),
            (FinishEffect::Border, 30., 4),
        ] {
            let style = effect.with_intensity(SelectionFinishStyle::default(), intensity);
            let layout = style.layout(24, 20).unwrap();
            assert_eq!(
                layout.content_rect,
                Rect {
                    x: padding,
                    y: padding,
                    width: 24,
                    height: 20
                }
            );
            assert_eq!(
                (layout.output_width, layout.output_height),
                (24 + 2 * padding, 20 + 2 * padding)
            );
            let output = style.compose(image(24, 20, [9, 19, 29, 255])).unwrap();
            assert_eq!(pixel(&output, padding + 12, padding + 10), [9, 19, 29, 255]);
            assert_eq!(output.readback_ms, 17.5);
        }
        let inside = FinishEffect::Border.with_intensity(
            SelectionFinishStyle {
                border_inside: true,
                ..Default::default()
            },
            100.,
        );
        let layout = inside.layout(24, 20).unwrap();
        assert!(layout.padding.is_empty());
        assert_eq!(layout.content_rect, Rect::full(24, 20));
    }

    #[test]
    fn finish_text_parameter_display_limits_and_unchanged_percent_keep_exact_style() {
        let style = SelectionFinishStyle {
            corner_radius: 17.25,
            shadow_blur: 11.5,
            shadow_opacity: 0.371239,
            border_thickness: 2.5,
            ..Default::default()
        };
        // 显示按 0.1% 取整；同值确认仍原样保留细小透明度（见下方 applied 断言）。
        assert_eq!(FinishField::Opacity.value(style), 37.1);
        for (field, limits) in [
            (FinishField::Radius, (0., 60.)),
            (FinishField::Blur, (0., 48.)),
            (FinishField::Opacity, (0., 100.)),
            (FinishField::Thickness, (0., 8.)),
        ] {
            assert_eq!(field.limits(), limits);
            assert_eq!(field.applied(style, field.value(style)), style);
            assert_eq!(field.applied(style, f32::NAN), style);
            assert_eq!(field.applied(style, f32::INFINITY), style);
            assert_eq!(field.value(field.applied(style, limits.1 + 100.)), limits.1);
            let lower = field.applied(style, limits.0 - 100.);
            if field == FinishField::Radius {
                assert!(!lower.corner_enabled);
                assert_eq!(lower.corner_radius, style.corner_radius);
            } else {
                assert_eq!(field.value(lower), limits.0);
            }
        }
        let changed = FinishField::Opacity.applied(style, 37.5);
        assert_eq!(changed.shadow_opacity, 0.375);
        assert_eq!(FinishField::Opacity.value(changed), 37.5);
    }

    #[test]
    fn finish_text_parameter_changes_only_requested_field_and_its_effect_switch() {
        let style = SelectionFinishStyle {
            corner_radius: 17.25,
            shadow_blur: 11.5,
            shadow_opacity: 0.371239,
            shadow_color: 0x80332211,
            border_color: 0xaabbccdd,
            border_thickness: 2.5,
            border_inside: true,
            remember_enabled: true,
            ..Default::default()
        };
        for (field, value, expected) in [
            (
                FinishField::Radius,
                9.5,
                SelectionFinishStyle {
                    corner_radius: 9.5,
                    corner_enabled: true,
                    ..style
                },
            ),
            (
                FinishField::Blur,
                7.25,
                SelectionFinishStyle {
                    shadow_blur: 7.25,
                    shadow_enabled: true,
                    ..style
                },
            ),
            (
                FinishField::Opacity,
                52.5,
                SelectionFinishStyle {
                    shadow_opacity: 0.525,
                    shadow_enabled: true,
                    ..style
                },
            ),
            (
                FinishField::Thickness,
                3.75,
                SelectionFinishStyle {
                    border_thickness: 3.75,
                    border_enabled: true,
                    ..style
                },
            ),
        ] {
            let actual = field.applied(style, value);
            assert_eq!(actual, expected, "{field:?}");
            assert_eq!(actual.normalized(), actual);
        }
        let enabled = SelectionFinishStyle {
            corner_enabled: true,
            shadow_enabled: true,
            border_enabled: true,
            ..style
        };
        assert_eq!(
            FinishField::Radius.applied(enabled, 0.),
            SelectionFinishStyle {
                corner_enabled: false,
                ..enabled
            }
        );
        // 0 即关，保留最后档位。
        assert_eq!(
            FinishField::Blur.applied(enabled, 0.),
            SelectionFinishStyle {
                shadow_enabled: false,
                ..enabled
            }
        );
        assert_eq!(
            FinishField::Thickness.applied(enabled, 0.),
            SelectionFinishStyle {
                border_enabled: false,
                ..enabled
            }
        );
        assert_eq!(
            FinishField::Opacity.applied(enabled, 0.),
            SelectionFinishStyle {
                shadow_opacity: 0.,
                shadow_enabled: false,
                ..enabled
            }
        );
    }

    #[test]
    fn finish_text_parameter_zero_radius_disables_without_consuming_last_value() {
        let original = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 17.25,
            ..Default::default()
        };
        let closed = FinishField::Radius.applied(original, 0.);
        assert_eq!(
            closed,
            SelectionFinishStyle {
                corner_enabled: false,
                ..original
            }
        );
        // 关闭时显示 0；档位留在模型里。
        assert_eq!(FinishField::Radius.value(closed), 0.);
        assert_eq!(closed.corner_radius, 17.25);
        assert_eq!(closed.geometry(80, 60).radius, 0.);
        let reopened = SelectionFinishStyle {
            corner_enabled: true,
            ..closed
        }
        .normalized();
        assert_eq!(reopened, original);
        assert_eq!(reopened.geometry(80, 60).radius, 17.25);
        // 从 0 拖到任意正值即开启；数字 0 不改变已经关闭的状态与档位。
        assert_eq!(FinishField::Radius.applied(closed, 17.25), original);
        assert_eq!(FinishField::Radius.applied(closed, 0.), closed);
    }

    #[test]
    fn finish_looks_are_snapshots_and_raw_settles_into_matching_look() {
        let looks = FinishLooks::default();
        for preset in FinishPreset::ALL {
            let style = looks.get(preset);
            assert_eq!(style, preset.defaults());
            assert_eq!(style.normalized(), style, "{preset:?}");
            // 出厂外观归类回自己，缩略图与名称一致。
            assert_eq!(FinishPreset::matching(style), preset, "{preset:?}");
            assert_eq!(preset.resolve(style), style, "{preset:?}");
            assert_eq!(FinishPreset::parse(preset.key()), Some(preset));
        }
        assert!(!looks.get(FinishPreset::Raw).has_effect());
        // 原样不存参数；其它槽位存规范化外观，不带 remember。
        let mut looks = looks;
        let tuned = SelectionFinishStyle {
            corner_radius: 30.,
            remember_enabled: true,
            ..FinishPreset::Card.defaults()
        };
        looks.set(FinishPreset::Raw, tuned);
        looks.set(FinishPreset::Card, tuned);
        assert_eq!(looks.get(FinishPreset::Raw), FinishPreset::Raw.defaults());
        assert_eq!(
            looks.get(FinishPreset::Card),
            SelectionFinishStyle {
                remember_enabled: false,
                ..tuned
            }
        );
        // 原样上拖出圆角切到圆角；卡片把阴影拖到 0 仍是卡片。
        let round = FinishField::Radius.applied(FinishPreset::Raw.defaults(), 20.);
        assert_eq!(FinishPreset::Raw.settle(round), FinishPreset::Round);
        assert_eq!(
            FinishPreset::Raw.settle(FinishPreset::Raw.defaults()),
            FinishPreset::Raw
        );
        let flat = FinishField::Blur.applied(FinishPreset::Card.defaults(), 0.);
        assert_eq!(FinishPreset::Card.settle(flat), FinishPreset::Card);
    }

    #[test]
    fn finish_text_parameter_fractional_border_keeps_content_origin_and_clipped_render() {
        let corner = FinishField::Radius.applied(SelectionFinishStyle::default(), 40.);
        assert_eq!(corner.corner_radius, 40.);
        assert_eq!(corner.geometry(24, 20).radius, 10.);
        let rounded = corner.compose(image(24, 20, [9, 19, 29, 255])).unwrap();
        assert_eq!(pixel(&rounded, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&rounded, 12, 10), [9, 19, 29, 255]);

        let outside = FinishField::Thickness.applied(corner, 2.5);
        let layout = outside.layout(24, 20).unwrap();
        assert_eq!(
            layout.content_rect,
            Rect {
                x: 3,
                y: 3,
                width: 24,
                height: 20
            }
        );
        assert_eq!((layout.output_width, layout.output_height), (30, 26));
        let output = outside.compose(image(24, 20, [9, 19, 29, 255])).unwrap();
        assert_eq!(
            pixel(
                &output,
                layout.content_rect.x + 12,
                layout.content_rect.y + 10
            ),
            [9, 19, 29, 255]
        );
        let full_border = outside.border_overlay(24, 20).unwrap();
        let region = Rect {
            x: 1,
            y: 2,
            width: 13,
            height: 11,
        };
        let cropped_border = outside.border_region(24, 20, region).unwrap();
        for y in 0..region.height {
            for x in 0..region.width {
                assert_eq!(
                    pixel(&cropped_border, x, y),
                    pixel(&full_border, x + region.x, y + region.y)
                );
            }
        }
        let inside = SelectionFinishStyle {
            border_inside: true,
            ..outside
        };
        let inside_layout = inside.layout(24, 20).unwrap();
        assert!(inside_layout.padding.is_empty());
        assert_eq!(inside_layout.content_rect, Rect::full(24, 20));
    }

    #[test]
    fn disabled_effects_return_original_allocation_and_metadata() {
        let source = image(8, 5, [1, 2, 3, 128]);
        let allocation = source.bgra.as_ptr();
        let output = SelectionFinishStyle::default().compose(source).unwrap();
        assert_eq!(output.bgra.as_ptr(), allocation);
        assert_eq!(
            (output.width, output.height, output.readback_ms),
            (8, 5, 17.5)
        );
        assert_eq!(pixel(&output, 0, 0), [1, 2, 3, 128]);
    }

    #[test]
    fn params_are_bounded_and_corner_toggle_keeps_last_radius() {
        let mut s = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 24.,
            shadow_enabled: true,
            shadow_blur: f32::INFINITY,
            shadow_opacity: f32::NAN,
            border_enabled: true,
            border_thickness: -3.,
            ..Default::default()
        }
        .normalized();
        assert_eq!(
            (s.shadow_blur, s.shadow_opacity, s.border_thickness),
            (48., 0., 1.)
        );
        s.corner_enabled = false;
        assert_eq!(s.geometry(50, 50).radius, 0.);
        s.corner_enabled = true;
        assert_eq!(s.geometry(50, 50).radius, 24.);
        assert_eq!(s.geometry(5, 9).radius, 2.5);
        // 记忆开关跨截图保留，是否生效交给样式。
        s.shadow_enabled = true;
        s.prepare_for_new_capture();
        assert!(s.shadow_enabled && s.corner_enabled);
        assert_eq!(s.corner_radius, 24.);
    }

    #[test]
    fn preset_resolves_fixed_effects_over_remembered_switches() {
        let memory = SelectionFinishStyle {
            corner_enabled: true,
            shadow_enabled: false,
            shadow_blur: 0.,
            ..Default::default()
        };
        let raw = FinishPreset::Raw.resolve(memory);
        assert!(raw.corner_enabled && !raw.shadow_enabled && !raw.border_enabled);
        let card = FinishPreset::Card.resolve(memory);
        assert!(card.corner_enabled && card.shadow_enabled && !card.border_enabled);
        assert!(card.shadow_blur > 0., "卡片自带阴影必须可见");
        let stroke = FinishPreset::Stroke.resolve(memory);
        assert!(stroke.corner_enabled && !stroke.shadow_enabled && stroke.border_enabled);
        let shadowed = FinishPreset::Stroke.resolve(SelectionFinishStyle {
            shadow_enabled: true,
            ..memory
        });
        assert!(shadowed.shadow_enabled && shadowed.border_enabled);
        for preset in FinishPreset::ALL {
            assert_eq!(FinishPreset::parse(preset.key()), Some(preset));
        }
        // 三个独立开关的任意组合都能由反推的样式原样还原。
        for bits in 0..8 {
            let style = SelectionFinishStyle {
                corner_enabled: bits & 1 != 0,
                shadow_enabled: bits & 2 != 0,
                border_enabled: bits & 4 != 0,
                ..Default::default()
            }
            .normalized();
            assert_eq!(
                FinishPreset::matching(style).resolve(style),
                style,
                "{bits}"
            );
        }
        let legacy = SelectionFinishStyle {
            remember_enabled: true,
            shadow_enabled: true,
            ..Default::default()
        };
        assert_eq!(FinishPreset::from_legacy(legacy), FinishPreset::Card);
        assert_eq!(
            FinishPreset::from_legacy(SelectionFinishStyle {
                remember_enabled: false,
                ..legacy
            }),
            FinishPreset::Raw
        );
    }

    #[test]
    fn padding_adds_outer_border_to_shadow_and_ignores_invisible_effects() {
        let mut s = SelectionFinishStyle {
            shadow_enabled: true,
            shadow_blur: 5.1,
            border_enabled: true,
            border_thickness: 7.25,
            ..Default::default()
        };
        // 阴影从外侧边框外沿投出：7.25 + 5.1 = 12.35，向外取整。
        assert_eq!(
            s.padding(),
            FinishPadding {
                left: 13,
                top: 13,
                right: 13,
                bottom: 13
            }
        );
        s.border_inside = true;
        assert_eq!(s.padding().left, 6);
        s.shadow_color = 0x0000_0000;
        assert!(s.padding().is_empty());
        s.border_inside = false;
        s.border_color = 0x00ff_ffff;
        assert!(s.padding().is_empty());
    }

    #[test]
    fn rounded_edges_keep_straight_rgb_and_opaque_center() {
        let s = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 4.,
            ..Default::default()
        };
        let source = image(12, 10, [15, 90, 220, 128]);
        let pointer = source.bgra.as_ptr();
        let output = s.compose(source).unwrap();
        assert_eq!(output.bgra.as_ptr(), pointer);
        assert_eq!(pixel(&output, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&output, 6, 5), [15, 90, 220, 128]);
        let partial = pixel(&output, 2, 0);
        assert!(partial[3] > 0 && partial[3] < 128);
        assert_eq!(&partial[..3], &[15, 90, 220]);
        assert_eq!(partial, pixel(&output, 9, 0));
    }

    #[test]
    fn small_odd_crops_are_masked_once_and_match_area_coverage() {
        for (w, h, radius) in [(1, 1, 12.), (3, 5, 12.), (7, 7, 3.5), (10, 8, 2.25)] {
            let s = SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: radius,
                ..Default::default()
            };
            let geometry = s.geometry(w, h);
            let output = s.compose(image(w, h, [13, 47, 209, 180])).unwrap();
            for y in 0..h {
                for x in 0..w {
                    let expected =
                        (180. * geometry.coverage_at(x as f64 + 0.5, y as f64 + 0.5)).round() as u8;
                    assert_eq!(pixel(&output, x, y)[3], expected, "{w}×{h}: ({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn inner_and_outer_border_preserve_content_coordinates() {
        let mut s = SelectionFinishStyle {
            border_enabled: true,
            border_inside: true,
            border_thickness: 1.,
            ..Default::default()
        };
        let inner = s.compose(image(5, 5, [0, 0, 255, 255])).unwrap();
        assert_eq!((inner.width, inner.height), (5, 5));
        assert_eq!(pixel(&inner, 0, 2), [255, 255, 255, 255]);
        assert_eq!(pixel(&inner, 2, 2), [0, 0, 255, 255]);
        s.border_inside = false;
        let layout = s.layout(5, 5).unwrap();
        assert_eq!(
            layout.content_rect,
            Rect {
                x: 1,
                y: 1,
                width: 5,
                height: 5
            }
        );
        let outer = s.compose(image(5, 5, [0, 0, 255, 255])).unwrap();
        assert_eq!((outer.width, outer.height), (7, 7));
        assert_eq!(pixel(&outer, 0, 3), [255, 255, 255, 255]);
        assert_eq!(pixel(&outer, 1, 3), [0, 0, 255, 255]);
        assert_eq!(outer.readback_ms, 17.5);
    }

    #[test]
    fn shared_corner_radius_keeps_border_square_when_content_radius_is_zero() {
        let square = SelectionFinishStyle {
            corner_radius: 0.,
            border_enabled: true,
            border_thickness: 3.,
            border_color: 0xff12_3456,
            ..Default::default()
        };
        let rounded = FinishField::Radius.applied(square, 4.);
        let disabled = FinishField::Radius.applied(rounded, 0.);
        assert_eq!(disabled.corner_radius, 4.);
        for original in [square, rounded, disabled] {
            for inside in [false, true] {
                let style = SelectionFinishStyle {
                    border_inside: inside,
                    ..original
                };
                let output = style.compose(image(15, 13, [23, 71, 119, 255])).unwrap();
                let expected = if style.geometry(15, 13).radius == 0. {
                    [0x56, 0x34, 0x12, 255]
                } else {
                    [0; 4]
                };
                for (x, y) in [
                    (0, 0),
                    (output.width - 1, 0),
                    (output.width - 1, output.height - 1),
                    (0, output.height - 1),
                ] {
                    assert_eq!(pixel(&output, x, y), expected, "{style:?}: ({x}, {y})");
                }
                assert_eq!(
                    pixel(&output, output.width / 2, output.height / 2),
                    [23, 71, 119, 255]
                );
            }
        }
    }

    #[test]
    fn border_antialias_tracks_independent_high_resolution_curved_area() {
        for (radius, thickness, inside, sx, sy, phase_x, phase_y) in [
            (4., 1., false, 1., 1., 0., 0.),
            (8., 1., true, 1., 1., 0., 0.),
            (8., 2.25, false, 1.375, 1.375, 0.25, 0.75),
            (12., 1., true, 1.25, 0.875, 0.25, 0.75),
        ] {
            let geometry = RoundedGeometry {
                width: 31,
                height: 25,
                radius,
            };
            let (inner, outer) = geometry.border_band(inside, thickness);
            let (pixel_width, pixel_height) = (1. / sx, 1. / sy);
            let mut area_error = 0.;
            let mut linear_error = 0.;
            let mut max_area_error: f64 = 0.;
            for row in -4..18 {
                for column in -4..18 {
                    let x = (f64::from(column) + 0.5 + phase_x) * pixel_width;
                    let y = (f64::from(row) + 0.5 + phase_y) * pixel_height;
                    // 独立参考按原距离带进行二维高倍栅格化，不复用横截面积分。
                    let mut covered = 0_u32;
                    for sample_y in 0..64 {
                        for sample_x in 0..64 {
                            let px = x + ((f64::from(sample_x) + 0.5) / 64. - 0.5) * pixel_width;
                            let py = y + ((f64::from(sample_y) + 0.5) / 64. - 0.5) * pixel_height;
                            let distance = geometry.border_distance(px, py);
                            covered += u32::from(inner <= distance && distance <= outer);
                        }
                    }
                    let reference = f64::from(covered) / (64. * 64.);
                    let actual = geometry.border_coverage_at(
                        x,
                        y,
                        inside,
                        thickness,
                        pixel_width,
                        pixel_height,
                    );
                    let sd = geometry.border_distance(x, y);
                    let epsilon = 0.0001;
                    let gx = (geometry.border_distance(x + epsilon, y)
                        - geometry.border_distance(x - epsilon, y))
                        / (2. * epsilon);
                    let gy = (geometry.border_distance(x, y + epsilon)
                        - geometry.border_distance(x, y - epsilon))
                        / (2. * epsilon);
                    let metric = (gx * pixel_width)
                        .hypot(gy * pixel_height)
                        .max(f64::EPSILON);
                    let old_linear = ((sd - inner) / metric + 0.5)
                        .min((outer - sd) / metric + 0.5)
                        .clamp(0., 1.);
                    area_error += (actual - reference).powi(2);
                    linear_error += (old_linear - reference).powi(2);
                    max_area_error = max_area_error.max((actual - reference).abs());
                }
            }
            assert!(
                max_area_error < 2. / 255.,
                "radius={radius}, scale={sx}/{sy}"
            );
            assert!(
                area_error < linear_error * 0.02,
                "radius={radius}, scale={sx}/{sy}: area={area_error}, linear={linear_error}"
            );
        }
    }

    #[test]
    fn border_antialias_keeps_fractional_and_subpixel_straight_edges_exact() {
        for radius in [0., 8.] {
            let geometry = RoundedGeometry {
                width: 31,
                height: 25,
                radius,
            };
            for inside in [false, true] {
                for thickness in [1., 2.25] {
                    let (start, end) = if inside {
                        (0., thickness)
                    } else {
                        (-thickness, 0.)
                    };
                    for (pixel_width, pixel_height) in [(1. / 1.375, 1. / 0.875), (8., 8.)] {
                        for y in [-2.75_f64, -0.35, 0.25, 1.75, 2.75] {
                            let expected = ((y + pixel_height * 0.5).min(end)
                                - (y - pixel_height * 0.5).max(start))
                            .max(0.)
                                / pixel_height;
                            let actual = geometry.border_coverage_at(
                                15.5,
                                y,
                                inside,
                                thickness,
                                pixel_width,
                                pixel_height,
                            );
                            assert!(
                                (actual - expected).abs() < 1e-12,
                                "radius={radius}, inside={inside}, width={thickness}, pixel={pixel_width}/{pixel_height}, y={y}: {actual} != {expected}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn translucent_border_blends_with_source_without_dark_fringe() {
        let s = SelectionFinishStyle {
            border_enabled: true,
            border_inside: true,
            border_thickness: 1.,
            border_color: 0x8000_ff00,
            ..Default::default()
        };
        let output = s.compose(image(5, 5, [0, 0, 255, 64])).unwrap();
        assert_eq!(pixel(&output, 0, 2), [0, 204, 51, 160]);
        assert_eq!(pixel(&output, 2, 2), [0, 0, 255, 64]);
    }

    #[test]
    fn shadow_is_symmetric_fades_and_does_not_fill_transparent_interior() {
        let s = SelectionFinishStyle {
            shadow_enabled: true,
            shadow_blur: 6.,
            shadow_color: 0xffff_0000,
            shadow_opacity: 1.,
            ..Default::default()
        };
        let output = s.compose(image(7, 7, [17, 23, 47, 0])).unwrap();
        assert_eq!(pixel(&output, 9, 9)[3], 0);
        assert_eq!(pixel(&output, 6, 9)[3], 0);
        let near = pixel(&output, 5, 9);
        let far = pixel(&output, 1, 9);
        assert!(near[3] > far[3] && far[3] > 0);
        assert_eq!(&near[..3], &[0, 0, 255]);
        assert_eq!(near, pixel(&output, 13, 9));
        assert_eq!(near, pixel(&output, 9, 5));
    }

    #[test]
    fn shadow_starts_at_outer_border_edge() {
        let shadow = SelectionFinishStyle {
            shadow_enabled: true,
            shadow_blur: 6.,
            shadow_color: 0xffff_0000,
            shadow_opacity: 1.,
            ..Default::default()
        };
        let both = SelectionFinishStyle {
            border_enabled: true,
            border_inside: false,
            border_thickness: 4.,
            border_color: 0xff00_ff00,
            ..shadow
        };
        // 外侧边框把阴影整体外推：留出边框宽度后仍有完整的阴影衰减。
        assert_eq!(both.padding().left, 10);
        let output = both.compose(image(7, 7, [17, 23, 47, 255])).unwrap();
        // 边框外沿外第一个像素就是阴影最浓处，与无边框时内容外第一个像素一致。
        let plain = shadow.compose(image(7, 7, [17, 23, 47, 255])).unwrap();
        assert_eq!(pixel(&output, 5, 13), pixel(&plain, 5, 9));
        assert_eq!(pixel(&output, 7, 13), [0, 255, 0, 255]);
        assert_eq!(pixel(&output, 0, 13)[3], 0);
        // 内侧边框不外推阴影。
        let inside = SelectionFinishStyle {
            border_inside: true,
            ..both
        };
        assert_eq!(inside.padding().left, 6);
        assert_eq!(inside.shadow_offset(), 0.);
    }

    #[test]
    fn cropped_preview_regions_equal_full_layers_pixel_for_pixel() {
        for inside in [true, false] {
            let s = SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 5.25,
                shadow_enabled: true,
                shadow_color: 0xa02f_658b,
                shadow_blur: 7.5,
                shadow_opacity: 0.72,
                border_enabled: true,
                border_color: 0x9099_2266,
                border_thickness: 2.25,
                border_inside: inside,
                ..Default::default()
            };
            let layout = s.layout(27, 19).unwrap();
            let (w, h) = (layout.output_width, layout.output_height);
            let full_shadow = s.shadow_overlay(27, 19).unwrap();
            let full_border = s.border_overlay(27, 19).unwrap();
            for region in [
                Rect {
                    x: 0,
                    y: 0,
                    width: w,
                    height: 12,
                },
                Rect {
                    x: 0,
                    y: h - 12,
                    width: w,
                    height: 12,
                },
                Rect {
                    x: 0,
                    y: 12,
                    width: 12,
                    height: h - 24,
                },
                Rect {
                    x: w - 12,
                    y: 12,
                    width: 12,
                    height: h - 24,
                },
                Rect {
                    x: 4,
                    y: 9,
                    width: 16,
                    height: 13,
                },
                Rect {
                    x: w - 1,
                    y: h - 1,
                    width: 1,
                    height: 1,
                },
            ] {
                for (part, full) in [
                    (s.shadow_region(27, 19, region).unwrap(), &full_shadow),
                    (s.border_region(27, 19, region).unwrap(), &full_border),
                ] {
                    assert_eq!((part.width, part.height), (region.width, region.height));
                    assert_eq!(
                        part.bgra.len(),
                        region.width as usize * region.height as usize * 4
                    );
                    for y in 0..region.height {
                        for x in 0..region.width {
                            assert_eq!(pixel(&part, x, y), pixel(full, region.x + x, region.y + y));
                        }
                    }
                }
            }
            assert!(s.shadow_region(27, 19, Rect::full(w + 1, h)).is_err());
            assert!(s
                .border_region(
                    27,
                    19,
                    Rect {
                        x: u32::MAX,
                        y: 0,
                        width: 1,
                        height: 1
                    }
                )
                .is_err());
        }
    }

    #[test]
    fn separate_preview_layers_match_export_with_transparent_and_partial_content() {
        let s = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 4.,
            shadow_enabled: true,
            shadow_color: 0x9900_5500,
            shadow_blur: 5.5,
            border_enabled: true,
            border_color: 0x80ee_3366,
            border_inside: false,
            border_thickness: 1.5,
            ..Default::default()
        };
        let mut source = image(11, 9, [153, 81, 211, 97]);
        source.bgra[(4 * 11 + 5) * 4..(4 * 11 + 5) * 4 + 4].fill(0);
        let original = source.bgra.clone();
        let output = s.compose(source).unwrap();
        let mut preview = s.shadow_overlay(11, 9).unwrap();
        let border = s.border_overlay(11, 9).unwrap();
        let layout = s.layout(11, 9).unwrap();
        for y in 0..9 {
            for x in 0..11 {
                let i = (y * 11 + x) as usize * 4;
                let mut src: [u8; 4] = original[i..i + 4].try_into().unwrap();
                src[3] = (src[3] as f64
                    * layout.geometry.coverage_at(x as f64 + 0.5, y as f64 + 0.5))
                .round() as u8;
                let d = ((y + layout.padding.top) * preview.width + x + layout.padding.left)
                    as usize
                    * 4;
                blend_over(&mut preview.bgra[d..d + 4], src);
            }
        }
        for (dst, src) in preview
            .bgra
            .chunks_exact_mut(4)
            .zip(border.bgra.chunks_exact(4))
        {
            blend_over(dst, src.try_into().unwrap());
        }
        assert_eq!(preview.bgra, output.bgra);
        assert_eq!(
            pixel(&output, layout.padding.left + 5, layout.padding.top + 4),
            [0, 0, 0, 0]
        );
    }

    #[test]
    fn malformed_images_and_unbounded_layouts_fail_before_allocation() {
        let s = SelectionFinishStyle::default();
        assert!(s.layout(0, 8).is_err());
        assert!(s.layout(u32::MAX, 8).is_err());
        assert!(s.layout(16_384, 16_384).is_err());
        assert!(s
            .compose(Image {
                width: 5,
                height: 5,
                bgra: vec![0; 99],
                readback_ms: 0.
            })
            .is_err());
        let expanded = SelectionFinishStyle {
            shadow_enabled: true,
            shadow_blur: 48.,
            ..Default::default()
        };
        // 原图恰好 256 MiB，但扩展输出越界，不能悄悄截断阴影。
        assert!(expanded.layout(8192, 8192).is_err());
    }

    #[test]
    fn presets_and_combined_overlay_keep_model_limits() {
        let mut s = SelectionFinishStyle::default();
        s.apply_shadow_preset(ShadowPreset::Standard);
        assert_eq!((s.shadow_blur, s.shadow_opacity), (16., 0.45));
        s.apply_shadow_preset(ShadowPreset::Strong);
        assert_eq!((s.shadow_blur, s.shadow_opacity), (28., 0.55));
        s.apply_shadow_preset(ShadowPreset::Soft);
        assert_eq!((s.shadow_blur, s.shadow_opacity), (10., 0.36));
        assert!(!s.has_effect());
        assert!(s.overlay(3, 3).unwrap().bgra.iter().all(|&b| b == 0));
    }
}
