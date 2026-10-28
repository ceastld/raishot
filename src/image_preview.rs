//! 图片编辑器的物理可见范围，以及与完整像素资格分离的采样预览。
use crate::{geometry::Rect, viewport::Viewport, Result};

#[path = "image_preview_geometry.rs"]
mod preview_geometry;
pub(crate) use preview_geometry::PreviewGeometry;
#[path = "image_preview_paint.rs"]
pub(crate) mod paint;
#[path = "image_preview_png.rs"]
pub(crate) mod png;
#[path = "image_preview_region_geometry.rs"]
pub(crate) mod region_geometry;
#[path = "image_preview_source.rs"]
pub(crate) mod source;
#[path = "image_preview_viewport.rs"]
pub(crate) mod viewport;

/// 逆变换客户区，保留两原图像素采样边；完全离屏不伪造边缘预览。
pub(crate) fn visible_image_region(
    full: Rect,
    viewport: Viewport,
    physical_client: Rect,
) -> Result<Option<Rect>> {
    let full_right = full.x.checked_add(full.width).ok_or("图片范围端点溢出")?;
    let full_bottom = full.y.checked_add(full.height).ok_or("图片范围端点溢出")?;
    let client_right = physical_client
        .x
        .checked_add(physical_client.width)
        .ok_or("图片客户区端点溢出")?;
    let client_bottom = physical_client
        .y
        .checked_add(physical_client.height)
        .ok_or("图片客户区端点溢出")?;
    if full.width == 0
        || full.height == 0
        || physical_client.width == 0
        || physical_client.height == 0
        || !viewport.zoom.is_finite()
        || viewport.zoom <= 0.
        || !viewport.offset.x.is_finite()
        || !viewport.offset.y.is_finite()
    {
        return Err("图片可见范围或视口无效".into());
    }
    // 用双精度完成逆变换，避免大偏移/小密度在单精度中间值溢出。
    let density = f64::from(viewport.zoom);
    let left = (f64::from(physical_client.x) - f64::from(viewport.offset.x)) / density;
    let top = (f64::from(physical_client.y) - f64::from(viewport.offset.y)) / density;
    let right = (f64::from(client_right) - f64::from(viewport.offset.x)) / density;
    let bottom = (f64::from(client_bottom) - f64::from(viewport.offset.y)) / density;
    // 逆坐标须能保留采样边的整数运算范围，不用饱和转换掩盖异常输入。
    if [left, top, right, bottom]
        .iter()
        .any(|v| !v.is_finite() || *v <= i64::MIN as f64 + 2. || *v >= i64::MAX as f64 - 2.)
        || right <= left
        || bottom <= top
    {
        return Err("图片逆变换坐标溢出或范围为空".into());
    }
    if right <= f64::from(full.x)
        || bottom <= f64::from(full.y)
        || left >= f64::from(full_right)
        || top >= f64::from(full_bottom)
    {
        return Ok(None);
    }
    // 先确认实际交集，再扩采样边；否则完全离屏会被两像素边误变成可见。
    let x = (left.floor() - 2.).max(f64::from(full.x)) as u32;
    let y = (top.floor() - 2.).max(f64::from(full.y)) as u32;
    let end_x = (right.ceil() + 2.).min(f64::from(full_right)) as u32;
    let end_y = (bottom.ceil() + 2.).min(f64::from(full_bottom)) as u32;
    Ok(Some(Rect {
        x,
        y,
        width: end_x - x,
        height: end_y - y,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::Point;

    #[test]
    fn long_image_editor_projected_viewport_region_uses_physical_literal_coordinates() {
        // 期望是独立固定表；client已是物理像素，helper没有可再乘的DPI参数。
        let cases = [
            (
                Rect::full(32000, 100),
                Viewport {
                    zoom: 1.,
                    offset: Point { x: -4090., y: -10. },
                },
                Rect::full(960, 64),
                Rect {
                    x: 4088,
                    y: 8,
                    width: 964,
                    height: 68,
                },
            ),
            (
                Rect::full(1000, 32000),
                Viewport {
                    zoom: 0.375,
                    offset: Point { x: 50., y: -3000. },
                },
                Rect::full(300, 120),
                Rect {
                    x: 0,
                    y: 7998,
                    width: 669,
                    height: 324,
                },
            ),
            // 0.37f32略大于37/100，37物理像素的逆值略小于100，故floor是99。
            (
                Rect::full(2000, 32000),
                Viewport {
                    zoom: 0.37,
                    offset: Point { x: 0., y: 0. },
                },
                Rect {
                    x: 37,
                    y: 74,
                    width: 370,
                    height: 185,
                },
                Rect {
                    x: 97,
                    y: 197,
                    width: 1005,
                    height: 505,
                },
            ),
            (
                Rect::full(1000, 32000),
                Viewport {
                    zoom: 2.,
                    offset: Point { x: -10., y: 30. },
                },
                Rect::full(960, 640),
                Rect {
                    x: 3,
                    y: 0,
                    width: 484,
                    height: 307,
                },
            ),
            (
                Rect::full(32000, 1000),
                Viewport {
                    zoom: 0.5,
                    offset: Point {
                        x: -7995.,
                        y: -100.,
                    },
                },
                Rect::full(1000, 400),
                Rect {
                    x: 15988,
                    y: 198,
                    width: 2004,
                    height: 802,
                },
            ),
            (
                Rect {
                    x: 100,
                    y: 200,
                    width: 1000,
                    height: 32000,
                },
                Viewport::default(),
                Rect {
                    x: 150,
                    y: 250,
                    width: 10,
                    height: 20,
                },
                Rect {
                    x: 148,
                    y: 248,
                    width: 14,
                    height: 24,
                },
            ),
            // 1200×720逻辑客户区已由宿主在125%DPI转换为1500×900，不能再放大。
            (
                Rect::full(32000, 960),
                Viewport {
                    zoom: 1.,
                    offset: Point {
                        x: -5000.,
                        y: -100.,
                    },
                },
                Rect::full(1500, 900),
                Rect {
                    x: 4998,
                    y: 98,
                    width: 1504,
                    height: 862,
                },
            ),
            (
                Rect::full(100, 100),
                Viewport {
                    zoom: 1.,
                    offset: Point { x: 99., y: 0. },
                },
                Rect::full(100, 100),
                Rect {
                    x: 0,
                    y: 0,
                    width: 3,
                    height: 100,
                },
            ),
        ];
        for (full, viewport, client, expected) in cases {
            assert_eq!(
                visible_image_region(full, viewport, client).unwrap(),
                Some(expected)
            );
        }
        for offset in [
            Point { x: 100., y: 0. },
            Point { x: -100., y: 0. },
            Point { x: 101., y: 0. },
            Point { x: 0., y: 101. },
            Point { x: 0., y: -100. },
        ] {
            assert_eq!(
                visible_image_region(
                    Rect::full(100, 100),
                    Viewport { zoom: 1., offset },
                    Rect::full(100, 100)
                )
                .unwrap(),
                None
            );
        }
        let full = Rect::full(1000, 32000);
        let client = Rect::full(1500, 900);
        for viewport in [
            Viewport {
                zoom: 0.,
                ..Viewport::default()
            },
            Viewport {
                zoom: -1.,
                ..Viewport::default()
            },
            Viewport {
                zoom: f32::NAN,
                ..Viewport::default()
            },
            Viewport {
                zoom: f32::INFINITY,
                ..Viewport::default()
            },
            Viewport {
                zoom: f32::MIN_POSITIVE,
                ..Viewport::default()
            },
            Viewport {
                zoom: 1.,
                offset: Point { x: f32::NAN, y: 0. },
            },
            Viewport {
                zoom: 1.,
                offset: Point {
                    x: 0.,
                    y: f32::INFINITY,
                },
            },
            Viewport {
                zoom: 1.,
                offset: Point { x: f32::MAX, y: 0. },
            },
        ] {
            assert!(visible_image_region(full, viewport, client).is_err());
        }
        let overflow = Rect {
            x: u32::MAX,
            y: 0,
            width: 1,
            height: 1,
        };
        let overflow_y = Rect {
            x: 0,
            y: u32::MAX,
            width: 1,
            height: 1,
        };
        for invalid in [Rect::full(0, 1), Rect::full(1, 0), overflow, overflow_y] {
            assert!(visible_image_region(invalid, Viewport::default(), client).is_err());
            assert!(visible_image_region(full, Viewport::default(), invalid).is_err());
        }
    }
}
