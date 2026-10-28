//! 内容坐标和窗口外观坐标的统一物理投影；辅助UI主题不参与。
use crate::{
    geometry::Rect, output_projection::OutputProjection, selection_finish::SelectionFinishStyle,
    viewport::Viewport, Result,
};

pub(crate) fn window_size(
    content: (u32, u32),
    style: Option<SelectionFinishStyle>,
) -> Result<(u32, u32)> {
    let Some(style) = style else {
        return Ok(content);
    };
    let size = {
        let layout = style.layout(content.0, content.1)?;
        (layout.output_width, layout.output_height)
    };
    OutputProjection::new(Rect::full(size.0, size.1), 1.)?;
    Ok(size)
}

/// outer等比contain，并把同一次padding映射到内容原点；body、OCR和输入共用。
pub(crate) fn content_viewport(
    content: (u32, u32),
    host: (u32, u32),
    style: Option<SelectionFinishStyle>,
) -> Viewport {
    // 创建/恢复/处理前已检查完整outer预算。异常数据不能生成极大窗口。
    let outer = window_size(content, style).unwrap_or(content);
    let mut viewport = crate::pin_zoom::projection(outer, host);
    if let Some(style) = style {
        let padding = style.padding();
        viewport.offset.x += padding.left as f32 * viewport.zoom;
        viewport.offset.y += padding.top as f32 * viewport.zoom;
    }
    viewport
}

/// 截图自然尺寸意图保留原始物理内容锚点；缩小外观放在整数 padding 内侧。
/// 旧恢复记录若客户区连原始左/上 padding 都容不下，保留其已有 contain 合同。
pub(crate) fn content_viewport_with_sizing(
    content: (u32, u32),
    host: (u32, u32),
    style: Option<SelectionFinishStyle>,
    sizing: crate::pin_model::PinWindowSizing,
) -> Viewport {
    if sizing == crate::pin_model::PinWindowSizing::General {
        return content_viewport(content, host, style);
    }
    let padding = style.map(|style| style.padding());
    let (left, top, right, bottom) = padding.map_or((0, 0, 0, 0), |padding| {
        (padding.left, padding.top, padding.right, padding.bottom)
    });
    if (left == 0 && top == 0 && right == 0 && bottom == 0) || host.0 <= left || host.1 <= top {
        return content_viewport(content, host, style);
    }
    let mut zoom = ((f64::from(host.0 - left) / (f64::from(content.0) + f64::from(right)).max(1.))
        .min(f64::from(host.1 - top) / (f64::from(content.1) + f64::from(bottom)).max(1.))
        .min(1.)) as f32;
    // f32 比例若向上取整，最多退一个 ULP，使缩放后整套外观也留在客户区内。
    if f64::from(left) + (f64::from(content.0) + f64::from(right)) * f64::from(zoom)
        > f64::from(host.0)
        || f64::from(top) + (f64::from(content.1) + f64::from(bottom)) * f64::from(zoom)
            > f64::from(host.1)
    {
        zoom = zoom.next_down();
    }
    Viewport {
        zoom,
        offset: crate::annotation::Point {
            x: left as f32,
            y: top as f32,
        },
    }
}

/// 保留完整来源坐标；可见域的左上角只在投影时减一次，不移动对象或采样相位。
pub(crate) fn source_content_viewport(
    content: Rect,
    host: (u32, u32),
    style: Option<SelectionFinishStyle>,
    sizing: crate::pin_model::PinWindowSizing,
) -> Viewport {
    let mut viewport =
        content_viewport_with_sizing((content.width, content.height), host, style, sizing);
    viewport.offset.x -= content.x as f32 * viewport.zoom;
    viewport.offset.y -= content.y as f32 * viewport.zoom;
    viewport
}

/// 转移显式缩放锚点时使用旧/新真实内容投影；普通路径沿用原计算。
pub(crate) fn anchored_placement(
    content: (u32, u32),
    style: Option<SelectionFinishStyle>,
    old: crate::pin_model::Placement,
    next: (u32, u32),
    anchor: (f32, f32),
    sizing: (
        crate::pin_model::PinWindowSizing,
        crate::pin_model::PinWindowSizing,
    ),
) -> crate::pin_model::Placement {
    if sizing.0 == crate::pin_model::PinWindowSizing::General
        && sizing.1 == crate::pin_model::PinWindowSizing::General
    {
        return crate::pin_zoom::anchored(
            old,
            window_size(content, style).unwrap_or(content),
            next,
            anchor,
        );
    }
    let before = content_viewport_with_sizing(content, (old.width, old.height), style, sizing.0);
    let after = content_viewport_with_sizing(content, next, style, sizing.1);
    let anchor = crate::annotation::Point {
        x: if anchor.0.is_finite() {
            anchor.0
        } else {
            before.offset.x
        },
        y: if anchor.1.is_finite() {
            anchor.1
        } else {
            before.offset.y
        },
    };
    let target = after.window_point(before.image_point(anchor));
    let shifted = |origin: i32, from: f32, to: f32| {
        (f64::from(origin) + f64::from(from) - f64::from(to))
            .round()
            .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    };
    crate::pin_model::Placement {
        x: shifted(old.x, anchor.x, target.x),
        y: shifted(old.y, anchor.y, target.y),
        width: next.0.max(1),
        height: next.1.max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Point;

    #[test]
    fn screenshot_one_pixel_border_preserves_signed_content_origin_at_first_show() {
        let style = Some(SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 1.,
            ..Default::default()
        });
        let outer = window_size((31, 25), style).unwrap();
        assert_eq!(outer, (33, 27));
        let viewport = content_viewport((31, 25), outer, style);
        let screen_outer = (-1902, -678);
        let local = viewport.window_point(Point { x: 0., y: 0. });
        assert_eq!(
            (
                screen_outer.0 + local.x as i32,
                screen_outer.1 + local.y as i32
            ),
            (-1901, -677)
        );
        assert_eq!(viewport.image_point(local), Point { x: 0., y: 0. });
    }

    #[test]
    fn odd_target_retains_contain_letterbox_and_all_content_owners_share_roundtrip() {
        let style = Some(SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 1.,
            ..Default::default()
        });
        for host in [(41, 34), (137, 89), (24, 180)] {
            let viewport = content_viewport((31, 25), host, style);
            let outer = crate::pin_zoom::projection((33, 27), host);
            assert_eq!(viewport.zoom, outer.zoom);
            assert_eq!(viewport.offset.x, outer.offset.x + outer.zoom);
            assert_eq!(viewport.offset.y, outer.offset.y + outer.zoom);
            for dpi in [1., 1.25, 1.5, 2.] {
                let original = Point { x: 7.5, y: 19.5 };
                let physical = viewport.window_point(original);
                let dip = Point {
                    x: physical.x / dpi,
                    y: physical.y / dpi,
                };
                let restored = viewport.image_point(Point {
                    x: dip.x * dpi,
                    y: dip.y * dpi,
                });
                assert!((original.x - restored.x).abs() < 0.00001);
                assert!((original.y - restored.y).abs() < 0.00001);
            }
        }
    }

    #[test]
    fn ordinary_pin_uses_the_existing_projection_and_outer_limit_is_checked() {
        assert_eq!(window_size((65536, 1024), None).unwrap(), (65536, 1024));
        let viewport = content_viewport((22, 100), (24, 100), None);
        assert_eq!(viewport.zoom, 1.);
        assert_eq!(viewport.offset, Point { x: 1., y: 0. });
        let style = Some(SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 1.,
            ..Default::default()
        });
        assert!(window_size((65535, 1), style).is_err());
        assert!(window_size((1000, 32000), style).is_err());
    }

    #[test]
    fn exact_capture_projection_preserves_padding_and_real_outer_plan_at_desktop_limit() {
        use crate::{
            pin_model::PinWindowSizing,
            pin_presentation::{PhysicalPoint, PresentationPreviewPlan},
        };
        let style = SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 8.,
            ..Default::default()
        };
        let before = content_viewport((16_384, 884), (16_384, 899), Some(style));
        assert!(before.offset.x - 8. > 1.);
        // 第二例的 f32 比例原本向上取整，真实 f64 outer 的 right 会越过 host。
        for (content, host) in [((16_384, 884), (16_384, 899)), ((16_384, 1), (16_384, 17))] {
            let viewport = content_viewport_with_sizing(
                content,
                host,
                Some(style),
                PinWindowSizing::ExactCapture,
            );
            assert_eq!(viewport.offset, Point { x: 8., y: 8. });
            assert!(viewport.zoom > 0. && viewport.zoom < 1.);
            let plan = PresentationPreviewPlan::new(
                Rect::full(content.0, content.1),
                style,
                PhysicalPoint {
                    x: f64::from(viewport.offset.x),
                    y: f64::from(viewport.offset.y),
                },
                f64::from(viewport.zoom),
            )
            .unwrap();
            assert_eq!(
                (plan.content_bounds().left, plan.content_bounds().top),
                (8., 8.)
            );
            let outer = plan.outer_bounds();
            assert!(outer.left >= 0. && outer.top >= 0.);
            assert!(outer.left + outer.width <= f64::from(host.0));
            assert!(outer.top + outer.height <= f64::from(host.1));
        }
    }

    #[test]
    fn exact_capture_natural_content_and_legacy_small_restore_keep_existing_geometry() {
        use crate::pin_model::PinWindowSizing;
        for thickness in [1., 8.] {
            let style = Some(SelectionFinishStyle {
                border_enabled: true,
                border_thickness: thickness,
                ..Default::default()
            });
            let host = window_size((31, 25), style).unwrap();
            assert_eq!(
                content_viewport_with_sizing((31, 25), host, style, PinWindowSizing::ExactCapture),
                content_viewport((31, 25), host, style)
            );
            for dpi in [1., 1.25, 1.5, 1.75, 2.] {
                let physical_host = (
                    (host.0 as f32 / dpi * dpi).round() as u32,
                    (host.1 as f32 / dpi * dpi).round() as u32,
                );
                let viewport = content_viewport_with_sizing(
                    (31, 25),
                    physical_host,
                    style,
                    PinWindowSizing::ExactCapture,
                );
                for outer in [(-1902, -678), (-8, 0), (2552, 113)] {
                    assert_eq!(
                        (
                            f64::from(outer.0) + f64::from(viewport.offset.x),
                            f64::from(outer.1) + f64::from(viewport.offset.y)
                        ),
                        (
                            f64::from(outer.0) + f64::from(thickness),
                            f64::from(outer.1) + f64::from(thickness)
                        )
                    );
                }
            }
        }
        // 旧合法 ExactCapture 关闭记录可以小于未缩放的 shadow padding。
        let legacy_style = Some(SelectionFinishStyle {
            shadow_enabled: true,
            shadow_blur: 48.,
            ..Default::default()
        });
        let legacy = content_viewport_with_sizing(
            (10, 10),
            (27, 27),
            legacy_style,
            PinWindowSizing::ExactCapture,
        );
        assert_eq!(legacy, content_viewport((10, 10), (27, 27), legacy_style));
        assert!(legacy.zoom.is_finite() && legacy.zoom > 0.);
        // 没有外观 padding 的旧合法恢复，包括留白/放大，沿用原投影。
        for (content, host) in [((601, 359), (240, 144)), ((10, 10), (20, 20))] {
            assert_eq!(
                content_viewport_with_sizing(content, host, None, PinWindowSizing::ExactCapture),
                content_viewport(content, host, None)
            );
        }
    }

    #[test]
    fn exact_capture_exit_uses_real_content_anchor_and_general_path_is_unchanged() {
        use crate::pin_model::{PinWindowSizing, Placement};
        let content = (16_384, 884);
        let style = Some(SelectionFinishStyle {
            border_enabled: true,
            border_thickness: 8.,
            ..Default::default()
        });
        let old = Placement {
            x: -16_392,
            y: -900,
            width: 16_384,
            height: 899,
        };
        let before = content_viewport_with_sizing(
            content,
            (old.width, old.height),
            style,
            PinWindowSizing::ExactCapture,
        );
        let next = (8200, 450);
        for anchor in [(8., 8.), (8192., 449.5), (14336.25, 811.75)] {
            let original = before.image_point(Point {
                x: anchor.0,
                y: anchor.1,
            });
            let placement = anchored_placement(
                content,
                style,
                old,
                next,
                anchor,
                (PinWindowSizing::ExactCapture, PinWindowSizing::General),
            );
            let after =
                content_viewport_with_sizing(content, next, style, PinWindowSizing::General);
            let drawn = after.window_point(original);
            let dx = f64::from(placement.x) + f64::from(drawn.x)
                - f64::from(old.x)
                - f64::from(anchor.0);
            let dy = f64::from(placement.y) + f64::from(drawn.y)
                - f64::from(old.y)
                - f64::from(anchor.1);
            assert!(
                dx.abs() <= 0.5 && dy.abs() <= 0.5,
                "窗口量化误差({dx},{dy})"
            );
        }
        assert_eq!(
            anchored_placement(
                content,
                style,
                old,
                (old.width, old.height),
                (8., 8.),
                (PinWindowSizing::ExactCapture, PinWindowSizing::ExactCapture)
            ),
            old
        );
        for (content, style, host, next) in [
            ((31, 25), style, (41, 34), (137, 89)),
            ((7, 11), None, (24, 24), (48, 52)),
        ] {
            let old = Placement {
                x: -1901,
                y: -677,
                width: host.0,
                height: host.1,
            };
            assert_eq!(
                anchored_placement(
                    content,
                    style,
                    old,
                    next,
                    (13.5, 17.25),
                    (PinWindowSizing::General, PinWindowSizing::General)
                ),
                crate::pin_zoom::anchored(
                    old,
                    window_size(content, style).unwrap(),
                    next,
                    (13.5, 17.25)
                )
            );
            assert_eq!(
                content_viewport_with_sizing(content, next, style, PinWindowSizing::General),
                content_viewport(content, next, style)
            );
        }
    }
}
