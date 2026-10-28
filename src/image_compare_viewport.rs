//! 同步对比视口与有界源像素采样；坐标均为视口逻辑坐标，缩放相对各自适配尺寸。
use crate::{
    capture::Image,
    image_tools::{
        allocate_pixels, check_cancel, validate_input, validate_output, ToolError, ToolLimits,
        ToolResult,
    },
};
use std::sync::atomic::AtomicBool;

pub const MAX_TILE_PIXELS: u64 = 2_000_000;
const MAX_TILE_SIDE: u32 = 8192;
const MIN_ZOOM: f64 = 1. / 65_536.;
const MAX_ZOOM: f64 = 65_536.;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CompareRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl CompareRect {
    fn valid(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width > 0.
            && self.height > 0.
            && (self.x + self.width).is_finite()
            && (self.y + self.height).is_finite()
            && self.x + self.width > self.x
            && self.y + self.height > self.y
    }

    fn contains(self, x: f64, y: f64) -> bool {
        x.is_finite()
            && y.is_finite()
            && x >= self.x
            && y >= self.y
            && x < self.x + self.width
            && y < self.y + self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CompareViewport {
    pub zoom: f64,
    pub center_x: f64,
    pub center_y: f64,
}

impl Default for CompareViewport {
    fn default() -> Self {
        Self {
            zoom: 1.,
            center_x: 0.5,
            center_y: 0.5,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CompareMapping {
    pub viewport: CompareRect,
    pub image: CompareRect,
    pub dimensions: (u32, u32),
}

impl CompareViewport {
    fn normalized(self) -> Option<Self> {
        if !self.zoom.is_finite()
            || !(MIN_ZOOM..=MAX_ZOOM).contains(&self.zoom)
            || !self.center_x.is_finite()
            || !self.center_y.is_finite()
        {
            return None;
        }
        Some(Self {
            center_x: self.center_x.clamp(0., 1.),
            center_y: self.center_y.clamp(0., 1.),
            ..self
        })
    }

    fn replace(&mut self, next: Self) -> bool {
        if *self == next {
            false
        } else {
            *self = next;
            true
        }
    }

    pub(crate) fn mapping(
        &self,
        dimensions: (u32, u32),
        rect: CompareRect,
    ) -> Option<CompareMapping> {
        let view = self.normalized()?;
        if dimensions.0 == 0 || dimensions.1 == 0 || !rect.valid() {
            return None;
        }
        let scale =
            (rect.width / f64::from(dimensions.0)).min(rect.height / f64::from(dimensions.1));
        let width = f64::from(dimensions.0) * scale * view.zoom;
        let height = f64::from(dimensions.1) * scale * view.zoom;
        let image = CompareRect {
            x: rect.x + rect.width * 0.5 - view.center_x * width,
            y: rect.y + rect.height * 0.5 - view.center_y * height,
            width,
            height,
        };
        image.valid().then_some(CompareMapping {
            viewport: rect,
            image,
            dimensions,
        })
    }

    /// dx/dy 表示图像随拖动移动的逻辑距离；两侧共享结果中的归一化中心。
    pub(crate) fn pan(
        &mut self,
        dimensions: (u32, u32),
        rect: CompareRect,
        dx: f64,
        dy: f64,
    ) -> bool {
        if !dx.is_finite() || !dy.is_finite() {
            return false;
        }
        let Some(view) = self.normalized() else {
            return false;
        };
        let Some(mapping) = view.mapping(dimensions, rect) else {
            return false;
        };
        self.replace(Self {
            center_x: (view.center_x - dx / mapping.image.width).clamp(0., 1.),
            center_y: (view.center_y - dy / mapping.image.height).clamp(0., 1.),
            ..view
        })
    }

    /// 尽可能固定指针下的源坐标；触及归一化中心边界时以中心范围为准。
    pub(crate) fn zoom_at(
        &mut self,
        dimensions: (u32, u32),
        rect: CompareRect,
        factor: f64,
        anchor: (f64, f64),
    ) -> bool {
        if !factor.is_finite() || factor <= 0. {
            return false;
        }
        let Some(view) = self.normalized() else {
            return false;
        };
        let Some(old) = view.mapping(dimensions, rect) else {
            return false;
        };
        if !rect.contains(anchor.0, anchor.1) {
            return false;
        }
        // 先比较比值再乘，有限但很大的 factor 不会溢出成无穷缩放。
        let zoom = if factor >= MAX_ZOOM / view.zoom {
            MAX_ZOOM
        } else if factor <= MIN_ZOOM / view.zoom {
            MIN_ZOOM
        } else {
            view.zoom * factor
        };
        if zoom == view.zoom {
            return self.replace(view);
        }
        let candidate = Self { zoom, ..view };
        let Some(new) = candidate.mapping(dimensions, rect) else {
            return false;
        };
        let offset_x = anchor.0 - (rect.x + rect.width * 0.5);
        let offset_y = anchor.1 - (rect.y + rect.height * 0.5);
        let next = Self {
            center_x: (view.center_x + offset_x / old.image.width - offset_x / new.image.width)
                .clamp(0., 1.),
            center_y: (view.center_y + offset_y / old.image.height - offset_y / new.image.height)
                .clamp(0., 1.),
            ..candidate
        };
        if next.mapping(dimensions, rect).is_none() {
            return false;
        }
        self.replace(next)
    }

    /// 当前侧一源像素对应一物理像素；另一侧仍使用同一相对适配倍数。
    /// 无法在有限缩放范围内达到该比例时不修改状态。
    pub(crate) fn actual(
        &mut self,
        dimensions: (u32, u32),
        rect: CompareRect,
        display_scale: f64,
    ) -> bool {
        if !display_scale.is_finite() || display_scale <= 0. {
            return false;
        }
        let Some(view) = self.normalized() else {
            return false;
        };
        let Some(fit) = (Self { zoom: 1., ..view }).mapping(dimensions, rect) else {
            return false;
        };
        let fit_scale = fit.image.width / f64::from(dimensions.0);
        let zoom = 1. / (fit_scale * display_scale);
        if !zoom.is_finite() || !(MIN_ZOOM..=MAX_ZOOM).contains(&zoom) {
            return false;
        }
        let next = Self { zoom, ..view };
        if next.mapping(dimensions, rect).is_none() {
            return false;
        }
        self.replace(next)
    }
}

impl CompareMapping {
    /// 连续源坐标以像素边界为整数；像素中心为 i+0.5，右/下边界及留白不命中。
    pub(crate) fn screen_to_source(self, x: f64, y: f64) -> Option<(f64, f64)> {
        if self.dimensions.0 == 0
            || self.dimensions.1 == 0
            || !self.viewport.valid()
            || !self.image.valid()
            || !self.viewport.contains(x, y)
            || !self.image.contains(x, y)
        {
            return None;
        }
        let source_x = (x - self.image.x) / self.image.width * f64::from(self.dimensions.0);
        let source_y = (y - self.image.y) / self.image.height * f64::from(self.dimensions.1);
        (source_x.is_finite()
            && source_y.is_finite()
            && source_x >= 0.
            && source_y >= 0.
            && source_x < f64::from(self.dimensions.0)
            && source_y < f64::from(self.dimensions.1))
        .then_some((source_x, source_y))
    }
}

/// 逻辑视口乘显示缩放得到所需像素；按统一比例缩小到硬上限，每边至少一像素。
pub(crate) fn tile_dimensions(
    width: f64,
    height: f64,
    display_scale: f64,
) -> Option<(u32, u32, bool)> {
    if !width.is_finite()
        || !height.is_finite()
        || !display_scale.is_finite()
        || width <= 0.
        || height <= 0.
        || display_scale <= 0.
    {
        return None;
    }
    let requested_width = (width * display_scale).ceil().max(1.);
    let requested_height = (height * display_scale).ceil().max(1.);
    if !requested_width.is_finite() || !requested_height.is_finite() {
        return None;
    }
    let side_scale = (f64::from(MAX_TILE_SIDE) / requested_width)
        .min(f64::from(MAX_TILE_SIDE) / requested_height)
        .min(1.);
    let side_width = (requested_width * side_scale).floor().max(1.) as u32;
    let side_height = (requested_height * side_scale).floor().max(1.) as u32;
    let pixels = u64::from(side_width) * u64::from(side_height);
    let pixel_scale = (MAX_TILE_PIXELS as f64 / pixels as f64).sqrt().min(1.);
    let output_width = (f64::from(side_width) * pixel_scale).floor().max(1.) as u32;
    let output_height = (f64::from(side_height) * pixel_scale).floor().max(1.) as u32;
    if output_width > MAX_TILE_SIDE
        || output_height > MAX_TILE_SIDE
        || u64::from(output_width) * u64::from(output_height) > MAX_TILE_PIXELS
    {
        return None;
    }
    Some((
        output_width,
        output_height,
        f64::from(output_width) < requested_width || f64::from(output_height) < requested_height,
    ))
}

/// 直接从冻结原帧采样当前裁区，只分配一张有界视口图，不先放大整图或缩略图。
pub(crate) fn render_view(
    image: &Image,
    mapping: CompareMapping,
    output: (u32, u32),
    cancel: &AtomicBool,
) -> ToolResult<Image> {
    check_cancel(cancel)?;
    validate_input(image, ToolLimits::default())?;
    if mapping.dimensions != (image.width, image.height)
        || !mapping.viewport.valid()
        || !mapping.image.valid()
    {
        return Err(ToolError::Invalid(
            tr!(
                "对比视口与源图不匹配",
                "Compare viewport does not match the source"
            )
            .into(),
        ));
    }
    let bytes = validate_output(
        output.0,
        output.1,
        ToolLimits {
            max_output_pixels: MAX_TILE_PIXELS,
            max_side: MAX_TILE_SIDE,
            ..ToolLimits::default()
        },
    )?;
    let mut bgra = allocate_pixels(bytes)?;
    let source_stride = image.width as usize * 4;
    let output_stride = output.0 as usize * 4;
    for y in 0..output.1 {
        check_cancel(cancel)?;
        let screen_y = mapping.viewport.y
            + (f64::from(y) + 0.5) / f64::from(output.1) * mapping.viewport.height;
        for x in 0..output.0 {
            let screen_x = mapping.viewport.x
                + (f64::from(x) + 0.5) / f64::from(output.0) * mapping.viewport.width;
            let Some((source_x, source_y)) = mapping.screen_to_source(screen_x, screen_y) else {
                continue;
            };
            let source = source_y.floor() as usize * source_stride + source_x.floor() as usize * 4;
            let destination = y as usize * output_stride + x as usize * 4;
            bgra[destination..destination + 4].copy_from_slice(&image.bgra[source..source + 4]);
        }
    }
    check_cancel(cancel)?;
    Ok(Image {
        width: output.0,
        height: output.1,
        bgra,
        readback_ms: 0.,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(width: f64, height: f64) -> CompareRect {
        CompareRect {
            x: 0.,
            y: 0.,
            width,
            height,
        }
    }

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    #[test]
    fn image_compare_viewport_aspects_keep_half_open_image_and_letterbox() {
        let bounds = CompareRect {
            x: 10.,
            y: 20.,
            ..rect(200., 100.)
        };
        let view = CompareViewport::default();
        let square = view.mapping((100, 100), bounds).unwrap();
        assert_eq!(
            square.image,
            CompareRect {
                x: 60.,
                y: 20.,
                width: 100.,
                height: 100.,
            }
        );
        assert_eq!(square.screen_to_source(60., 20.), Some((0., 0.)));
        assert_eq!(square.screen_to_source(110., 70.), Some((50., 50.)));
        for point in [(59., 70.), (160., 70.), (110., 120.), (210., 70.)] {
            assert_eq!(square.screen_to_source(point.0, point.1), None);
        }
        assert_eq!(view.mapping((200, 100), bounds).unwrap().image, bounds);
        let tall = view.mapping((100, 200), bounds).unwrap();
        assert_eq!((tall.image.x, tall.image.width), (85., 50.));
    }

    #[test]
    fn image_compare_viewport_zoom_anchor_and_pan_preserve_source_position() {
        let mut view = CompareViewport::default();
        let bounds = rect(200., 100.);
        let anchor = (150., 75.);
        let source = view
            .mapping((400, 200), bounds)
            .unwrap()
            .screen_to_source(anchor.0, anchor.1)
            .unwrap();
        assert!(view.zoom_at((400, 200), bounds, 2., anchor));
        close(view.center_x, 0.625);
        close(view.center_y, 0.625);
        let after = view
            .mapping((400, 200), bounds)
            .unwrap()
            .screen_to_source(anchor.0, anchor.1)
            .unwrap();
        close(after.0, source.0);
        close(after.1, source.1);
        assert!(view.pan((400, 200), bounds, 20., -10.));
        let dragged = view
            .mapping((400, 200), bounds)
            .unwrap()
            .screen_to_source(anchor.0 + 20., anchor.1 - 10.)
            .unwrap();
        close(dragged.0, source.0);
        close(dragged.1, source.1);
        assert!(view.pan((400, 200), bounds, 1e9, -1e9));
        assert_eq!((view.center_x, view.center_y), (0., 1.));
        assert!(!view.pan((400, 200), bounds, 1e9, -1e9));
    }

    #[test]
    fn image_compare_viewport_different_images_share_normalized_center_and_zoom() {
        let mut view = CompareViewport::default();
        let left = rect(400., 200.);
        let right = CompareRect { x: 500., ..left };
        assert!(view.pan((400, 200), left, -40., 20.));
        assert!(view.zoom_at((400, 200), left, 3., (200., 100.)));
        for (dimensions, bounds, center) in [
            ((400, 200), left, (200., 100.)),
            ((200, 400), right, (700., 100.)),
        ] {
            let point = view
                .mapping(dimensions, bounds)
                .unwrap()
                .screen_to_source(center.0, center.1)
                .unwrap();
            close(point.0 / f64::from(dimensions.0), view.center_x);
            close(point.1 / f64::from(dimensions.1), view.center_y);
        }
        assert_eq!(view.zoom, 3.);
    }

    #[test]
    fn image_compare_viewport_actual_uses_current_side_and_physical_display_scale() {
        let bounds = rect(200., 100.);
        for display_scale in [1., 1.5, 2.] {
            let mut view = CompareViewport {
                zoom: 3.,
                ..CompareViewport::default()
            };
            assert!(view.actual((400, 200), bounds, display_scale));
            let mapping = view.mapping((400, 200), bounds).unwrap();
            close(mapping.image.width / 400. * display_scale, 1.);
            let center = mapping.screen_to_source(100., 50.).unwrap();
            let next = mapping
                .screen_to_source(100. + 1. / display_scale, 50.)
                .unwrap();
            close(next.0 - center.0, 1.);
            let other = view.mapping((100, 400), bounds).unwrap();
            close(other.image.width / 100. * display_scale, 0.5);
            assert!(!view.actual((400, 200), bounds, display_scale));
        }
    }

    #[test]
    fn image_compare_viewport_invalid_numbers_and_tile_budgets_stay_bounded() {
        let bounds = rect(200., 100.);
        let mut view = CompareViewport::default();
        assert!(view.mapping((0, 1), bounds).is_none());
        assert!(view
            .mapping(
                (100, 100),
                CompareRect {
                    x: f64::NAN,
                    ..bounds
                },
            )
            .is_none());
        assert!(view
            .mapping((100, 100), bounds)
            .unwrap()
            .screen_to_source(f64::NAN, 50.)
            .is_none());
        for value in [f64::NAN, f64::INFINITY, 0., -1.] {
            assert!(tile_dimensions(value, 100., 1.).is_none());
            assert!(tile_dimensions(100., 100., value).is_none());
            let before = view;
            assert!(!view.zoom_at((100, 100), bounds, value, (100., 50.)));
            assert!(!view.actual((100, 100), bounds, value));
            assert_eq!(view, before);
        }
        assert!(!view.pan((100, 100), bounds, f64::NAN, 0.));
        assert!(CompareViewport {
            center_x: f64::NAN,
            ..view
        }
        .mapping((100, 100), bounds)
        .is_none());
        assert!(CompareViewport { zoom: 0., ..view }
            .mapping((100, 100), bounds)
            .is_none());
        assert_eq!(tile_dimensions(80., 40., 2.), Some((160, 80, false)));
        for (width, height, scale) in [(1000., 2000., 2.), (1e100, 1e100, 1.), (1e9, 1., 1.)] {
            let (width, height, reduced) = tile_dimensions(width, height, scale).unwrap();
            assert!(width > 0 && width <= MAX_TILE_SIDE);
            assert!(height > 0 && height <= MAX_TILE_SIDE);
            assert!(u64::from(width) * u64::from(height) <= MAX_TILE_PIXELS);
            assert!(reduced);
        }
        assert!(tile_dimensions(1e308, 100., 1e308).is_none());
        assert!(view.zoom_at((100, 100), bounds, 1e300, (100., 50.)));
        assert_eq!(view.zoom, MAX_ZOOM);
        assert!(view.zoom_at((100, 100), bounds, 1e-300, (100., 50.)));
        assert_eq!(view.zoom, MIN_ZOOM);
    }

    #[test]
    fn image_compare_viewport_render_samples_real_crop_alpha_and_cancellation() {
        let image = Image {
            width: 8,
            height: 4,
            bgra: (0..4_u8)
                .flat_map(|y| {
                    (0..8_u8).flat_map(move |x| [x, y, x + y, [0, 64, 255][(x + y) as usize % 3]])
                })
                .collect(),
            readback_ms: 42.,
        };
        let original = image.bgra.clone();
        let bounds = rect(4., 4.);
        let crop = CompareViewport {
            zoom: 2.,
            ..CompareViewport::default()
        }
        .mapping((8, 4), bounds)
        .unwrap();
        let cancel = AtomicBool::new(false);
        let rendered = render_view(&image, crop, (4, 4), &cancel).unwrap();
        for y in 0..4_usize {
            for x in 0..4_usize {
                let actual = (y * 4 + x) * 4;
                let source = (y * 8 + x + 2) * 4;
                assert_eq!(
                    &rendered.bgra[actual..actual + 4],
                    &original[source..source + 4]
                );
            }
        }
        let fit = CompareViewport::default().mapping((8, 4), bounds).unwrap();
        let letterbox = render_view(&image, fit, (4, 4), &cancel).unwrap();
        assert!(letterbox.bgra[..16].iter().all(|byte| *byte == 0));
        assert!(letterbox.bgra[48..].iter().all(|byte| *byte == 0));
        assert_eq!(&letterbox.bgra[16..20], &original[36..40]);
        assert!(matches!(
            render_view(&image, crop, (4, 4), &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
        for output in [(0, 1), (MAX_TILE_SIDE + 1, 1), (8192, 245)] {
            assert!(render_view(&image, crop, output, &cancel).is_err());
        }
        assert!(render_view(
            &image,
            CompareMapping {
                dimensions: (4, 8),
                ..crop
            },
            (4, 4),
            &cancel,
        )
        .is_err());
        assert_eq!(image.bgra, original);
        assert_eq!(image.readback_ms, 42.);
    }
}
