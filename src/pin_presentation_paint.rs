//! 实际目标尺度的 GPUI 外观几何；不持有或拉伸来源装饰位图。
//! shadow 使用有界同心外侧环带逼近共享 cubic falloff，最终像素仍需真实 GPU 核验。
use super::{PhysicalPoint, PresentationContour, PresentationPreviewPlan};
use crate::{
    annotation::{Point, Style},
    vector_mesh::VectorMesh,
    vector_paint::{FramePaths, PathPart},
};
use gpui::{point, px, rgba, FillOptions, FillRule, PathBuilder, PathStyle, Pixels, Window};

fn ring_path(
    outer: PresentationContour,
    inner: PresentationContour,
    origin: gpui::Point<Pixels>,
    dpi: f64,
) -> Option<gpui::Path<Pixels>> {
    let mut path = PathBuilder::fill().with_style(PathStyle::Fill(
        FillOptions::default().with_fill_rule(FillRule::EvenOdd),
    ));
    for contour in [outer, inner] {
        let b = contour.bounds;
        // 厚内边框可覆盖整张内容；内轮廓耗尽后不再挖孔。
        if b.width <= 0. || b.height <= 0. {
            continue;
        }
        let rx = contour.radius_x.min(b.width * 0.5).max(0.);
        let ry = contour.radius_y.min(b.height * 0.5).max(0.);
        let at = |x: f64, y: f64| {
            let p = PhysicalPoint { x, y }.to_dip(dpi)?;
            let (x, y) = (p.x as f32, p.y as f32);
            (x.is_finite() && y.is_finite()).then(|| origin + point(px(x), px(y)))
        };
        let right = b.left + b.width;
        let bottom = b.top + b.height;
        if rx == 0. || ry == 0. {
            // 无圆角内轮廓直接用矩形，避免向 SVG arc 提交零半径或重复端点。
            path.move_to(at(b.left, b.top)?);
            path.line_to(at(right, b.top)?);
            path.line_to(at(right, bottom)?);
            path.line_to(at(b.left, bottom)?);
            path.close();
            continue;
        }
        let radii = point(px((rx / dpi) as f32), px((ry / dpi) as f32));
        path.move_to(at(b.left + rx, b.top)?);
        path.line_to(at(right - rx, b.top)?);
        path.arc_to(radii, px(0.), false, true, at(right, b.top + ry)?);
        path.line_to(at(right, bottom - ry)?);
        path.arc_to(radii, px(0.), false, true, at(right - rx, bottom)?);
        path.line_to(at(b.left + rx, bottom)?);
        path.arc_to(radii, px(0.), false, true, at(b.left, bottom - ry)?);
        path.line_to(at(b.left, b.top + ry)?);
        path.arc_to(radii, px(0.), false, true, at(b.left + rx, b.top)?);
        path.close();
    }
    path.build().ok()
}

pub(crate) fn paint_shadow(
    plan: PresentationPreviewPlan,
    origin: gpui::Point<Pixels>,
    window: &mut Window,
) {
    let style = plan.style();
    if !style.shadow_enabled
        || style.shadow_blur <= 0.
        || style.shadow_opacity <= 0.
        || style.shadow_color >> 24 == 0
    {
        return;
    }
    let dpi = f64::from(window.scale_factor());
    let blur = f64::from(style.shadow_blur);
    // 外侧边框时阴影从边框外沿开始衰减，边框下方按峰值铺满（会被边框盖住）。
    let offset = style.shadow_offset();
    let width = plan.contour(blur).bounds.width - plan.contour(0.).bounds.width;
    let count = (width * 0.5).ceil().clamp(16., 192.) as u32;
    let inner = plan.contour(0.);
    let mut accumulated = 0.;
    // 从最外层到最内层。重叠环带采用 source-over 反解增量 alpha，不重复压暗。
    for index in (0..count).rev() {
        let t = (f64::from(index) + 0.5) / f64::from(count);
        let desired = f64::from(style.shadow_opacity) * f64::from((style.shadow_color >> 24) as u8)
            / 255.
            * (1. - t).powi(3)
            * (1. + 2. * t);
        let increment = ((desired - accumulated) / (1. - accumulated)).clamp(0., 1.);
        let alpha = (increment * 255.).round() as u32;
        if alpha > 0 {
            let outward = offset + blur * f64::from(index + 1) / f64::from(count);
            if let Some(path) = ring_path(plan.contour(outward), inner, origin, dpi) {
                window.paint_path(
                    path,
                    rgba(((style.shadow_color & 0x00ff_ffff) << 8) | alpha),
                );
            }
            accumulated += (1. - accumulated) * f64::from(alpha) / 255.;
        }
    }
}

pub(crate) fn paint_border(
    plan: PresentationPreviewPlan,
    origin: gpui::Point<Pixels>,
    window: &mut Window,
) {
    let style = plan.style();
    if !style.border_enabled || style.border_color >> 24 == 0 {
        return;
    }
    let (mut outer, mut inner) = plan.border_contours();
    let thickness = inner.bounds.left - outer.bounds.left;
    if thickness <= 0. {
        return;
    }
    // 小于一设备像素的线保持中心不变，以覆盖率表示线宽，避免两侧 AA 重叠而虚胖。
    let opacity = thickness.min(1.);
    if thickness < 1. {
        let extra = (1. - thickness) * 0.5;
        outer = offset_contour(outer, extra);
        inner = offset_contour(inner, -extra);
    }
    let mut contours = vec![border_polygon(outer)];
    if inner.bounds.width > 0. && inner.bounds.height > 0. {
        let mut hole = border_polygon(inner);
        hole.reverse();
        contours.push(hole);
    }
    if let Ok(mesh) = VectorMesh::fill_contours(&contours) {
        // 复用标注路径的连续解析边缘和覆盖率并集；普通填充环只有 4× MSAA 的四档透明度。
        let dpi = window.scale_factor();
        let border_style = Style {
            color: style.border_color & 0x00ff_ffff,
            opacity: (opacity * f64::from((style.border_color >> 24) as u8) / 255.) as f32,
            width: 1.,
            ..Default::default()
        };
        let path =
            FramePaths::new(1, origin, dpi, dpi).get(0, PathPart::Main, &mesh, border_style, false);
        if let Some(path) = path {
            let alpha = (border_style.opacity * 255.).round() as u32;
            let color = rgba((border_style.color << 8) | alpha);
            #[cfg(target_os = "windows")]
            window.paint_path_analytic_union(path, color);
            #[cfg(not(target_os = "windows"))]
            window.paint_path(path, color);
        }
    }
}

fn offset_contour(mut contour: PresentationContour, outward: f64) -> PresentationContour {
    contour.bounds.left -= outward;
    contour.bounds.top -= outward;
    contour.bounds.width += outward * 2.;
    contour.bounds.height += outward * 2.;
    let radius = |r: f64| if r > 0. { (r + outward).max(0.) } else { 0. };
    contour.radius_x = radius(contour.radius_x);
    contour.radius_y = radius(contour.radius_y);
    contour
}

fn border_polygon(contour: PresentationContour) -> Vec<Point> {
    let b = contour.bounds;
    let rx = contour.radius_x.min(b.width * 0.5).max(0.);
    let ry = contour.radius_y.min(b.height * 0.5).max(0.);
    let point = |x: f64, y: f64| Point {
        x: x as f32,
        y: y as f32,
    };
    if rx == 0. || ry == 0. {
        return vec![
            point(b.left, b.top),
            point(b.left + b.width, b.top),
            point(b.left + b.width, b.top + b.height),
            point(b.left, b.top + b.height),
        ];
    }
    // 按最终设备像素细分，圆弧弦误差不超过 0.01px；不随来源尺寸放大折线误差。
    let step = (2. * (1. - 0.01 / rx.max(ry)).clamp(-1., 1.).acos()).max(0.001);
    let count = (std::f64::consts::FRAC_PI_2 / step).ceil().max(2.) as usize;
    let mut points = Vec::with_capacity((count + 1) * 4);
    for (cx, cy, start) in [
        (
            b.left + b.width - rx,
            b.top + ry,
            -std::f64::consts::FRAC_PI_2,
        ),
        (b.left + b.width - rx, b.top + b.height - ry, 0.),
        (
            b.left + rx,
            b.top + b.height - ry,
            std::f64::consts::FRAC_PI_2,
        ),
        (b.left + rx, b.top + ry, std::f64::consts::PI),
    ] {
        for index in 0..=count {
            let angle = start + std::f64::consts::FRAC_PI_2 * index as f64 / count as f64;
            points.push(point(cx + rx * angle.cos(), cy + ry * angle.sin()));
        }
    }
    points
}
