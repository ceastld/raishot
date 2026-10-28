//! GPUI 初建窗口只用所选显示器的逻辑坐标，不用虚拟桌面物理范围猜 DPI。
//! 宿主必须同时设置该显示器的 DisplayId，并在工厂里用原生物理坐标定位全屏。
//! show=false 会保留 GPUI initial_placement；以后 activate_window 会重新应用它，
//! 因此宿主需要先消费该放置任务再做最终定位，不能仅隐藏后直接 SetWindowPos。
use gpui::{point, px, size, Bounds, Pixels};

const MAX_WIDTH: f32 = 320.;
const MAX_HEIGHT: f32 = 240.;

/// 给初建 renderer 一个位于指定显示器中心的小窗口，避免物理尺寸再次乘 DPI。
/// 只转换逻辑显示器自身范围；混合 DPI、负原点和屏间空隙由最终原生定位处理。
/// 无效显示器范围返回 None，调用方可使用 GPUI 自身的默认放置。
pub(crate) fn initial_bounds(display: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
    let x = f32::from(display.origin.x);
    let y = f32::from(display.origin.y);
    let width = f32::from(display.size.width);
    let height = f32::from(display.size.height);
    if ![x, y, width, height, x + width, y + height]
        .into_iter()
        .all(f32::is_finite)
        || width < 1.
        || height < 1.
    {
        return None;
    }
    let initial_width = width.min(MAX_WIDTH);
    let initial_height = height.min(MAX_HEIGHT);
    let initial_x = x + (width - initial_width) / 2.;
    let initial_y = y + (height - initial_height) / 2.;
    if !initial_x.is_finite() || !initial_y.is_finite() {
        return None;
    }
    Some(Bounds::new(
        point(px(initial_x), px(initial_y)),
        size(px(initial_width), px(initial_height)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display(x: f32, y: f32, width: f32, height: f32, dpi: f32) -> Bounds<Pixels> {
        Bounds::new(
            point(px(x / dpi), px(y / dpi)),
            size(px(width / dpi), px(height / dpi)),
        )
    }

    fn assert_inside(initial: Bounds<Pixels>, monitor: Bounds<Pixels>) {
        assert!(initial.left() >= monitor.left());
        assert!(initial.top() >= monitor.top());
        assert!(initial.right() <= monitor.right());
        assert!(initial.bottom() <= monitor.bottom());
        assert_eq!(initial.center(), monitor.center());
    }

    #[test]
    fn mixed_dpi_displays_keep_the_selected_monitor_center_in_physical_space() {
        for (monitor, dpi, physical_center) in [
            (display(0., 0., 2560., 1440., 1.25), 1.25, (1280., 720.)),
            (display(2560., 0., 1920., 1080., 1.), 1., (3520., 540.)),
        ] {
            let initial = initial_bounds(monitor).unwrap();
            assert_inside(initial, monitor);
            assert_eq!(f32::from(initial.center().x) * dpi, physical_center.0);
            assert_eq!(f32::from(initial.center().y) * dpi, physical_center.1);
            // 任一屏初建资源都保持小尺寸，不会以整个 4480px 桌面作为 DIP。
            assert!(f32::from(initial.size.width) * dpi <= 400.);
            assert!(f32::from(initial.size.height) * dpi <= 300.);
        }
    }

    #[test]
    fn negative_rotated_monitor_is_used_even_when_virtual_center_is_in_a_gap() {
        // 主屏 (0,0,1920,1080)，副屏左上方竖屏；虚拟中心 (420,-540) 在空隙。
        let rotated = display(-1080., -2160., 1080., 1920., 1.25);
        let initial = initial_bounds(rotated).unwrap();
        assert_inside(initial, rotated);
        assert_eq!(f32::from(initial.center().x) * 1.25, -540.);
        assert_eq!(f32::from(initial.center().y) * 1.25, -1200.);
        assert!(initial.right() < px(0.));
        assert!(initial.bottom() < px(0.));
    }

    #[test]
    fn small_and_invalid_displays_do_not_create_an_out_of_bounds_or_non_finite_window() {
        let small = display(-160., 90., 160., 90., 1.);
        assert_eq!(initial_bounds(small), Some(small));
        for invalid in [
            display(0., 0., 0., 1080., 1.),
            display(0., 0., 1920., 0., 1.),
            display(f32::NAN, 0., 1920., 1080., 1.),
            display(0., f32::INFINITY, 1920., 1080., 1.),
            display(0., 0., f32::INFINITY, 1080., 1.),
        ] {
            assert_eq!(initial_bounds(invalid), None);
        }
    }
}
