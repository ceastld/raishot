//! OCR 界面几何统一由原图坐标投到客户区 DIP；不修改原图、选区或文档。
use crate::{
    annotation::{Bounds, Point},
    geometry::Rect,
    toolbar_layout::PanelRect,
    viewport::Viewport,
};
use std::sync::Arc;

/// 全文/缺几何结果的真实 paint 租约；独立代次阻止布局 A→B→A 复活。
pub(crate) struct ProjectionPaint {
    pub(crate) epoch: Arc<()>,
    pub(crate) projection: OcrProjection,
}

impl ProjectionPaint {
    pub(crate) fn current(&self, epoch: &Arc<()>, projection: Option<OcrProjection>) -> bool {
        Arc::ptr_eq(&self.epoch, epoch) && projection == Some(self.projection)
    }
}

/// 同时作为真实 paint 的布局身份，缩放、平移、DPI 或客户区变化不能沿用旧回执。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OcrProjection {
    viewport: Viewport,
    dpi: f32,
    client: [f32; 2],
}

impl OcrProjection {
    pub(crate) fn new(viewport: Viewport, dpi: f32, client: [f32; 2]) -> Option<Self> {
        (dpi.is_finite()
            && dpi > 0.
            && viewport.zoom.is_finite()
            && viewport.zoom > 0.
            && viewport.offset.x.is_finite()
            && viewport.offset.y.is_finite()
            && client.iter().all(|value| value.is_finite() && *value > 0.))
        .then_some(Self {
            viewport,
            dpi,
            client,
        })
    }

    pub(crate) fn point(self, source: Point) -> Point {
        let physical = self.viewport.window_point(source);
        Point {
            x: physical.x / self.dpi,
            y: physical.y / self.dpi,
        }
    }

    /// 输入点是窗口 DIP；保留小数，不在原位选字前取整。
    pub(crate) fn source_point(self, client: Point) -> Point {
        self.viewport.image_point(Point {
            x: client.x * self.dpi,
            y: client.y * self.dpi,
        })
    }

    pub(crate) fn bounds(self, source: Bounds) -> PanelRect {
        let origin = self.point(Point {
            x: source.x,
            y: source.y,
        });
        PanelRect {
            x: origin.x,
            y: origin.y,
            width: source.width * self.viewport.zoom / self.dpi,
            height: source.height * self.viewport.zoom / self.dpi,
        }
    }

    pub(crate) fn rect(self, source: Rect) -> PanelRect {
        self.bounds(Bounds {
            x: source.x as f32,
            y: source.y as f32,
            width: source.width as f32,
            height: source.height as f32,
        })
    }

    /// 图片编辑的 UI 工作区只随客户区改变，不随图像缩放或平移。
    pub(crate) fn client_work(self, top: f32, bottom: f32) -> PanelRect {
        let top = top.clamp(0., (self.client[1] - 1.).max(0.));
        let bottom = bottom.clamp(0., (self.client[1] - top - 1.).max(0.));
        PanelRect {
            x: 0.,
            y: top,
            width: self.client[0],
            height: self.client[1] - top - bottom,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection(zoom: f32, offset: Point, dpi: f32) -> OcrProjection {
        OcrProjection::new(Viewport { zoom, offset }, dpi, [1060., 780.]).unwrap()
    }

    #[test]
    fn full_text_paint_lease_rejects_dpi_client_aba_and_preserves_its_current_epoch() {
        let original = projection(1., Point { x: 0., y: 0. }, 1.5);
        let old_epoch = Arc::new(());
        let old = ProjectionPaint {
            epoch: Arc::clone(&old_epoch),
            projection: original,
        };
        assert!(old.current(&old_epoch, Some(original)));
        // 两组 DPI/DIP 的物理客户区相同，仍不是同一布局。
        let changed = OcrProjection::new(Viewport::default(), 2., [795., 585.]).unwrap();
        assert_ne!(original, changed);
        assert!(!old.current(&old_epoch, Some(changed)));
        let new_epoch = Arc::new(());
        assert!(!old.current(&new_epoch, Some(original)));
        let new = ProjectionPaint {
            epoch: Arc::clone(&new_epoch),
            projection: original,
        };
        assert!(new.current(&new_epoch, Some(original)));
        assert!(!new.current(&old_epoch, Some(original)));
        assert!(!new.current(&new_epoch, None));
    }

    fn close(actual: Point, expected: Point) {
        assert!(
            actual.distance(expected) < 0.001,
            "{actual:?} != {expected:?}"
        );
    }

    #[test]
    fn screen_projection_preserves_existing_dip_and_nonzero_source_crop() {
        let p = projection(1., Point { x: 0., y: 0. }, 1.5);
        let crop = Rect {
            x: 60,
            y: 90,
            width: 120,
            height: 45,
        };
        assert_eq!(
            p.rect(crop),
            PanelRect {
                x: 40.,
                y: 60.,
                width: 80.,
                height: 30.,
            }
        );
        close(
            p.source_point(Point { x: 44., y: 64. }),
            Point { x: 66., y: 96. },
        );
    }

    #[test]
    fn image_quad_and_input_roundtrip_include_zoom_pan_and_high_dpi() {
        let source = Point {
            x: 301.25,
            y: 511.5,
        };
        for dpi in [1., 1.25, 1.5, 2., 3.] {
            let p = projection(
                0.375,
                Point {
                    x: -73.5,
                    y: 117.25,
                },
                dpi,
            );
            let expected = Point {
                x: (-73.5 + source.x * 0.375) / dpi,
                y: (117.25 + source.y * 0.375) / dpi,
            };
            close(p.point(source), expected);
            close(p.source_point(expected), source);
        }
    }

    #[test]
    fn global_mark_bounds_project_without_subtracting_crop_origin_again() {
        let p = projection(2., Point { x: 35., y: -20. }, 1.25);
        let bounds = p.bounds(Bounds {
            x: 101.,
            y: 205.,
            width: 80.,
            height: 12.,
        });
        assert_eq!(bounds.x, 189.6);
        assert_eq!(bounds.y, 312.);
        assert_eq!(bounds.width, 128.);
        assert_eq!(bounds.height, 19.2);
    }

    #[test]
    fn zoom_only_change_invalidates_paint_identity_even_with_same_offset() {
        let before = projection(0.5, Point { x: 0., y: 0. }, 1.5);
        let after = projection(1., Point { x: 0., y: 0. }, 1.5);
        assert_ne!(before, after);
        let source = Point { x: 400., y: 300. };
        assert_ne!(before.point(source), after.point(source));
    }

    #[test]
    fn client_controls_keep_fixed_dip_work_area_across_zoom_and_pan() {
        let before = projection(0.01, Point { x: 30., y: 40. }, 2.);
        let after = projection(
            8.,
            Point {
                x: -13000.,
                y: -4000.,
            },
            2.,
        );
        assert_eq!(before.client_work(92., 8.), after.client_work(92., 8.));
        assert_eq!(
            before.client_work(92., 8.),
            PanelRect {
                x: 0.,
                y: 92.,
                width: 1060.,
                height: 680.,
            }
        );
    }

    #[test]
    fn dpi_and_client_resize_each_change_paint_identity() {
        let before = projection(1., Point { x: 5., y: 7. }, 1.);
        assert_ne!(before, projection(1., Point { x: 5., y: 7. }, 2.));
        let resized = OcrProjection::new(
            Viewport {
                zoom: 1.,
                offset: Point { x: 5., y: 7. },
            },
            1.,
            [1061., 780.],
        )
        .unwrap();
        assert_ne!(before, resized);
    }

    #[test]
    fn tiny_client_keeps_ui_work_inside_actual_client() {
        let p = OcrProjection::new(Viewport::default(), 2., [20., 30.]).unwrap();
        assert_eq!(
            p.client_work(92., 8.),
            PanelRect {
                x: 0.,
                y: 29.,
                width: 20.,
                height: 1.
            }
        );
    }

    #[test]
    fn invalid_viewport_dpi_or_client_cannot_create_a_paint_receipt() {
        for dpi in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(OcrProjection::new(Viewport::default(), dpi, [100., 100.]).is_none());
        }
        for zoom in [0., -1., f32::NAN, f32::INFINITY] {
            assert!(OcrProjection::new(
                Viewport {
                    zoom,
                    ..Viewport::default()
                },
                1.,
                [100., 100.]
            )
            .is_none());
        }
        for client in [[0., 1.], [1., 0.], [f32::NAN, 1.], [1., f32::INFINITY]] {
            assert!(OcrProjection::new(Viewport::default(), 1., client).is_none());
        }
        assert!(OcrProjection::new(
            Viewport {
                offset: Point { x: f32::NAN, y: 0. },
                ..Viewport::default()
            },
            1.,
            [1., 1.]
        )
        .is_none());
    }
}
