//! 尺寸/外观条布局；只把已投影的物理像素几何转换一次到窗口 DIP。
use crate::{
    geometry::Rect, selection_action_layout::SelectionActionLayout,
    selection_chrome::SelectionChrome, toolbar_layout::PanelRect,
};

const HEIGHT: f32 = 34.;
const AVOID_GAP: f32 = 4.;
const CORNER_INSET: f32 = 12.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionBarDisplay {
    /// 样式按钮带样式名。
    Details,
    Dimensions,
    /// 样式按钮只留缩略图；样式面板始终可达。
    Compact,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SelectionBarWidths {
    pub details: f32,
    pub dimensions: f32,
    pub compact: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SelectionBarLayout {
    pub panel: PanelRect,
    pub display: SelectionBarDisplay,
    monitor: PanelRect,
    /// 选区在窗口 DIP 中的范围，外观面板据此优先放到选区外。
    selection: PanelRect,
}

fn valid(rect: PanelRect) -> bool {
    rect.x.is_finite()
        && rect.y.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite()
        && rect.width > 0.
        && rect.height > 0.
        && rect.right().is_finite()
        && rect.bottom().is_finite()
}

fn overlap(panel: PanelRect, avoid: &[PanelRect]) -> f32 {
    avoid
        .iter()
        .copied()
        .filter(|rect| valid(*rect))
        .map(|rect| {
            let width = (panel.right().min(rect.right() + AVOID_GAP)
                - panel.x.max(rect.x - AVOID_GAP))
            .max(0.);
            let height = (panel.bottom().min(rect.bottom() + AVOID_GAP)
                - panel.y.max(rect.y - AVOID_GAP))
            .max(0.);
            width * height
        })
        .sum()
}

/// 正数向外扩，负数向内缩；内缩到无面积时返回零宽高。
fn grow(rect: PanelRect, by: f32) -> PanelRect {
    PanelRect {
        x: rect.x - by,
        y: rect.y - by,
        width: (rect.width + 2. * by).max(0.),
        height: (rect.height + 2. * by).max(0.),
    }
}

fn intersects(a: PanelRect, b: PanelRect) -> bool {
    a.right().min(b.right()) - a.x.max(b.x) > 0.01
        && a.bottom().min(b.bottom()) - a.y.max(b.y) > 0.01
}

fn contains(outer: PanelRect, inner: PanelRect) -> bool {
    inner.x >= outer.x - 0.01
        && inner.y >= outer.y - 0.01
        && inner.right() <= outer.right() + 0.01
        && inner.bottom() <= outer.bottom() + 0.01
}

/// 点到矩形的最短距离；点在矩形内为 0。
fn distance(rect: PanelRect, x: f32, y: f32) -> f32 {
    let dx = (rect.x - x).max(0.).max(x - rect.right());
    let dy = (rect.y - y).max(0.).max(y - rect.bottom());
    dx.hypot(dy)
}

/// 方位罚分：上方最自然，其次左外侧、下方、右外侧；压在选区内挡内容，只作外面放不下时的退路。
fn side_penalty(panel: PanelRect, selection: PanelRect, inside: bool) -> f32 {
    if inside {
        400.
    } else if panel.bottom() <= selection.y + 0.01 {
        0.
    } else if panel.right() <= selection.x + 0.01 {
        24.
    } else if panel.y >= selection.bottom() - 0.01 {
        32.
    } else {
        40.
    }
}

impl SelectionBarLayout {
    /// 候选位置按“到选区左上角的最短距离 + 方位罚分”打分，取最小者。
    ///
    /// 硬约束：在显示器内；与工具栏、属性栏、OCR 面板留出间距；不骑在选区边框上——
    /// 要么在选区外留出控制点间距，要么完全在选区内侧。控制点不再单独当障碍，
    /// 否则贴着工具栏的位置会因零点几像素相交被否决，条被赶到屏幕角落。
    pub(crate) fn new(
        monitor: Rect,
        chrome: &SelectionChrome,
        ui_scale: f32,
        avoid: &[PanelRect],
        widths: SelectionBarWidths,
    ) -> Self {
        let scale = if ui_scale.is_finite() && ui_scale >= 1. / 64. {
            ui_scale
        } else {
            1.
        };
        let monitor = PanelRect {
            x: monitor.x as f32 / scale,
            y: monitor.y as f32 / scale,
            width: monitor.width as f32 / scale,
            height: monitor.height as f32 / scale,
        };
        let crop = chrome.0;
        let crop_left = crop.left as f32 / scale;
        let crop_top = crop.top as f32 / scale;
        let crop_right = crop.right as f32 / scale;
        let crop_bottom = crop.bottom as f32 / scale;
        let selection = PanelRect {
            x: crop_left,
            y: crop_top,
            width: (crop_right - crop_left).max(0.),
            height: (crop_bottom - crop_top).max(0.),
        };
        let edge = crate::selection_chrome::HANDLE_SIZE as f32 / 2. / scale + AVOID_GAP;
        let outer = grow(selection, edge);
        let inner = grow(selection, -edge);
        let blocked: Vec<PanelRect> = avoid.iter().copied().filter(|r| valid(*r)).collect();
        let height = HEIGHT.min(monitor.height);
        let choices = [
            (SelectionBarDisplay::Details, widths.details, 0.),
            (SelectionBarDisplay::Dimensions, widths.dimensions, 8.),
            (SelectionBarDisplay::Compact, widths.compact, 16.),
        ];
        let mut best: Option<(f32, Self)> = None;
        let mut fallback: Option<((f32, f32), Self)> = None;
        for (display, desired, display_penalty) in choices {
            let desired = if desired.is_finite() && desired > 0. {
                desired
            } else {
                widths.compact.max(1.)
            };
            if desired > monitor.width && display != SelectionBarDisplay::Compact {
                continue;
            }
            let width = desired.min(monitor.width);
            let clamp_x = |x: f32| x.clamp(monitor.x, (monitor.right() - width).max(monitor.x));
            let clamp_y = |y: f32| y.clamp(monitor.y, (monitor.bottom() - height).max(monitor.y));
            let mut points = Vec::new();
            // 选区上下：与选区左/右缘对齐，或紧贴某个障碍物的左右边。
            let mut xs = vec![selection.x, selection.right() - width];
            let mut ys = vec![selection.y, selection.bottom() - height];
            for rect in &blocked {
                xs.extend([rect.right() + AVOID_GAP, rect.x - AVOID_GAP - width]);
                ys.extend([rect.bottom() + AVOID_GAP, rect.y - AVOID_GAP - height]);
            }
            for &x in &xs {
                points.push((x, selection.y - edge - height));
                points.push((x, selection.bottom() + edge));
            }
            // 选区左右外侧：与选区上/下缘对齐，或贴着障碍物上下边。
            for &y in &ys {
                points.push((selection.x - edge - width, y));
                points.push((selection.right() + edge, y));
            }
            // 叠在障碍物上方/下方。
            for rect in &blocked {
                for x in [selection.x, rect.x, rect.right() - width] {
                    points.push((x, rect.y - AVOID_GAP - height));
                    points.push((x, rect.bottom() + AVOID_GAP));
                }
            }
            // 选区内侧四角，外面都放不下时才用。
            for x in [inner.x, inner.right() - width] {
                for y in [inner.y, inner.bottom() - height] {
                    points.push((x, y));
                }
            }
            // 显示器角落：最后手段。
            for x in [
                monitor.x + CORNER_INSET,
                monitor.right() - width - CORNER_INSET,
            ] {
                for y in [
                    monitor.y + CORNER_INSET,
                    monitor.bottom() - height - CORNER_INSET,
                ] {
                    points.push((x, y));
                }
            }
            for (x, y) in points {
                if !x.is_finite() || !y.is_finite() {
                    continue;
                }
                let panel = PanelRect {
                    x: clamp_x(x),
                    y: clamp_y(y),
                    width,
                    height,
                };
                let placed = Self {
                    panel,
                    display,
                    monitor,
                    selection,
                };
                let inside = valid(inner) && contains(inner, panel);
                let straddles = intersects(panel, outer) && !inside;
                let score = distance(panel, selection.x, selection.y)
                    + side_penalty(panel, selection, inside)
                    + display_penalty;
                let covered = overlap(panel, &blocked);
                if covered == 0. && !straddles {
                    if best.as_ref().map_or(true, |(s, _)| score < *s) {
                        best = Some((score, placed));
                    }
                } else {
                    // 全部受阻时保留可访问入口：先少挡障碍物，再少压选区边框，再近。
                    let key = (covered + if straddles { 1. } else { 0. }, score);
                    if fallback.as_ref().map_or(true, |(k, _)| key < *k) {
                        fallback = Some((key, placed));
                    }
                }
            }
        }
        best.map(|(_, placed)| placed)
            .or(fallback.map(|(_, placed)| placed))
            .unwrap_or(Self {
                panel: PanelRect {
                    x: monitor.x,
                    y: monitor.y,
                    width: widths.compact.min(monitor.width).max(0.),
                    height,
                },
                display: SelectionBarDisplay::Compact,
                monitor,
                selection,
            })
    }

    pub(crate) fn actions(self) -> SelectionActionLayout {
        SelectionActionLayout::anchored(self.panel, self.monitor)
    }

    /// 外观面板不挡正在预览的圆角：条外侧 → 选区左 → 选区右，最后才贴着条盖住选区。
    /// `anchor_x` 是效果按钮组左缘，面板与它左对齐。
    pub(crate) fn style_popup(self, width: f32, height: f32, anchor_x: f32) -> PanelRect {
        let monitor = self.monitor;
        let width = if width.is_finite() {
            width.clamp(0., monitor.width)
        } else {
            monitor.width
        };
        let height = if height.is_finite() {
            height.clamp(0., monitor.height)
        } else {
            monitor.height
        };
        let clamp_x = |x: f32| x.clamp(monitor.x, (monitor.right() - width).max(monitor.x));
        let clamp_y = |y: f32| y.clamp(monitor.y, (monitor.bottom() - height).max(monitor.y));
        let bar = self.panel;
        let selection = self.selection;
        let anchor = clamp_x(if anchor_x.is_finite() {
            anchor_x
        } else {
            bar.x
        });
        let rect = |x, y| PanelRect {
            x,
            y,
            width,
            height,
        };
        let above = bar.y - AVOID_GAP - height;
        let below = bar.bottom() + AVOID_GAP;
        if bar.bottom() <= selection.y + 0.5 && above >= monitor.y {
            return rect(anchor, above);
        }
        if bar.y + 0.5 >= selection.bottom() && below + height <= monitor.bottom() {
            return rect(anchor, below);
        }
        let side_y = clamp_y(bar.y.min(selection.y).max(monitor.y));
        let left = selection.x - AVOID_GAP - width;
        if left >= monitor.x {
            return rect(left, side_y);
        }
        let right = selection.right() + AVOID_GAP;
        if right + width <= monitor.right() {
            return rect(right, side_y);
        }
        if below + height <= monitor.bottom() {
            rect(anchor, below)
        } else {
            rect(anchor, clamp_y(above))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths() -> SelectionBarWidths {
        SelectionBarWidths {
            details: 430.,
            dimensions: 300.,
            compact: 190.,
        }
    }

    fn crop(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    fn layout(
        monitor: Rect,
        selected: Rect,
        scale: f32,
        avoid: &[PanelRect],
    ) -> SelectionBarLayout {
        SelectionBarLayout::new(
            monitor,
            &SelectionChrome::projected(selected, 1., (0., 0.)),
            scale,
            avoid,
            widths(),
        )
    }

    fn inside(bar: SelectionBarLayout, monitor: Rect, scale: f32) {
        assert!(bar.panel.x >= monitor.x as f32 / scale - 0.01);
        assert!(bar.panel.y >= monitor.y as f32 / scale - 0.01);
        assert!(bar.panel.right() <= (monitor.x as f32 + monitor.width as f32) / scale + 0.01);
        assert!(bar.panel.bottom() <= (monitor.y as f32 + monitor.height as f32) / scale + 0.01);
    }

    #[test]
    fn selection_bar_prefers_above_and_is_not_squeezed_to_a_tiny_selection() {
        let monitor = Rect::full(1200, 900);
        let bar = layout(monitor, crop(100, 200, 1, 1), 1., &[]);
        assert_eq!(bar.display, SelectionBarDisplay::Details);
        assert_eq!(bar.panel.width, 430.);
        assert_eq!(bar.panel.x, 100.);
        assert!(bar.panel.bottom() < 200.);
        inside(bar, monitor, 1.);
    }

    #[test]
    fn selection_bar_uses_below_when_the_selection_touches_the_top() {
        let monitor = Rect::full(1200, 900);
        let selected = crop(100, 0, 500, 300);
        let bar = layout(monitor, selected, 1., &[]);
        assert!(bar.panel.y > 300.);
        inside(bar, monitor, 1.);
    }

    #[test]
    fn selection_bar_fullscreen_fallback_avoids_all_eight_handles_and_main_toolbar() {
        let monitor = Rect::full(1000, 700);
        let main = PanelRect {
            x: 100.,
            y: 640.,
            width: 750.,
            height: 44.,
        };
        let bar = layout(monitor, monitor, 1., &[main]);
        assert_eq!(overlap(bar.panel, &[main]), 0.);
        let chrome = SelectionChrome::projected(monitor, 1., (0., 0.));
        for handle in chrome.handles() {
            let b = handle.bounds();
            let blocked = PanelRect {
                x: b.left as f32,
                y: b.top as f32,
                width: (b.right - b.left) as f32,
                height: (b.bottom - b.top) as f32,
            };
            assert_eq!(overlap(bar.panel, &[blocked]), 0.);
        }
        inside(bar, monitor, 1.);
    }

    #[test]
    fn selection_bar_keeps_entries_when_ocr_covers_the_preferred_position() {
        let monitor = Rect::full(1100, 800);
        let ocr = PanelRect {
            x: 0.,
            y: 0.,
            width: 550.,
            height: 500.,
        };
        let toolbar = PanelRect {
            x: 250.,
            y: 600.,
            width: 700.,
            height: 44.,
        };
        let bar = layout(monitor, crop(100, 250, 400, 300), 1., &[ocr, toolbar]);
        assert_eq!(overlap(bar.panel, &[ocr, toolbar]), 0.);
        inside(bar, monitor, 1.);
    }

    #[test]
    fn selection_bar_drops_output_then_radius_by_measured_width() {
        for (width, expected) in [
            (600, SelectionBarDisplay::Details),
            (360, SelectionBarDisplay::Dimensions),
            (240, SelectionBarDisplay::Compact),
        ] {
            let monitor = Rect::full(width, 800);
            let bar = layout(monitor, crop(0, 100, 1, 1), 1., &[]);
            assert_eq!(bar.display, expected);
            inside(bar, monitor, 1.);
        }
    }

    #[test]
    fn selection_bar_projects_fractional_zoom_and_negative_offset_only_once() {
        let monitor = crop(1000, 0, 1600, 1200);
        let selected = crop(850, 200, 100, 80);
        let chrome = SelectionChrome::projected(selected, 1.25, (-12.5, 20.25));
        let bar = SelectionBarLayout::new(monitor, &chrome, 1.5, &[], widths());
        assert!((bar.panel.x - chrome.0.left as f32 / 1.5).abs() < 0.01);
        assert!(
            (bar.panel.bottom() - (chrome.0.top as f32 / 1.5 - 6. / 1.5 - AVOID_GAP)).abs() < 0.01
        );
        inside(bar, monitor, 1.5);
    }

    fn disjoint(a: PanelRect, b: PanelRect) -> bool {
        a.right() <= b.x || b.right() <= a.x || a.bottom() <= b.y || b.bottom() <= a.y
    }

    #[test]
    fn style_popup_prefers_outside_the_selection() {
        let monitor = Rect::full(1600, 1000);
        let dip = |r: Rect| PanelRect {
            x: r.x as f32,
            y: r.y as f32,
            width: r.width as f32,
            height: r.height as f32,
        };
        // 常规：条在选区上方，面板放在条上方。
        let selected = crop(500, 500, 400, 300);
        let bar = layout(monitor, selected, 1., &[]);
        let popup = bar.style_popup(296., 326., bar.panel.x + 120.);
        assert!(popup.bottom() <= bar.panel.y);
        assert!(disjoint(popup, dip(selected)));
        // 贴顶：条退到选区下方/角落，面板改放选区左侧。
        let selected = crop(500, 0, 400, 300);
        let bar = layout(monitor, selected, 1., &[]);
        let popup = bar.style_popup(296., 326., bar.panel.x);
        assert!(disjoint(popup, dip(selected)));
        // 高选区贴顶贴左：条进角落/选区内，左侧放不下，改放右侧。
        let selected = crop(0, 0, 400, 900);
        let bar = layout(monitor, selected, 1., &[]);
        let popup = bar.style_popup(296., 326., bar.panel.x);
        assert!(disjoint(popup, dip(selected)));
        assert!(popup.x >= 400.);
        // 全屏：无处可放时仍在显示器内。
        let bar = layout(monitor, monitor, 1., &[]);
        let popup = bar.style_popup(296., 326., bar.panel.x);
        assert!(popup.x >= 0. && popup.right() <= 1600.);
        assert!(popup.y >= 0. && popup.bottom() <= 1000.);
    }

    /// 截图里的情况：选区靠下，主工具栏放在选区上方占住了“上方”。
    /// 旧算法把条推到工具栏右边或屏幕角落；现在应贴在选区左外侧、顶边对齐。
    #[test]
    fn selection_bar_stays_next_to_the_selection_when_the_toolbar_takes_above() {
        let monitor = Rect::full(1280, 720);
        let selected = crop(520, 470, 430, 236);
        let toolbar = PanelRect {
            x: 250.,
            y: 412.,
            width: 700.,
            height: 48.,
        };
        let bar = layout(monitor, selected, 1., &[toolbar]);
        assert_eq!(overlap(bar.panel, &[toolbar]), 0.);
        assert!(bar.panel.right() <= 520. - 6. - AVOID_GAP + 0.01);
        assert_eq!(bar.panel.y, 470.);
        assert!(distance(bar.panel, 520., 470.) < 20.);
        inside(bar, monitor, 1.);
    }

    /// 贴着工具栏的位置不因控制点被否决：选区右上角紧挨工具栏右端时，条仍在选区附近。
    #[test]
    fn selection_bar_beside_the_toolbar_is_not_rejected_by_corner_handles() {
        let monitor = Rect::full(1600, 900);
        let selected = crop(40, 500, 600, 380);
        let toolbar = PanelRect {
            x: 0.,
            y: 440.,
            width: 640.,
            height: 48.,
        };
        let bar = layout(monitor, selected, 1., &[toolbar]);
        assert_eq!(overlap(bar.panel, &[toolbar]), 0.);
        assert!(distance(bar.panel, 40., 500.) < 700.);
        assert!(bar.panel.y > 300., "不能退到屏幕顶部角落：{:?}", bar.panel);
        inside(bar, monitor, 1.);
    }

    /// 任意位置都不骑在选区边框上：要么留出控制点间距在外，要么完全在内侧。
    #[test]
    fn selection_bar_never_straddles_the_selection_edge() {
        let monitor = Rect::full(1280, 720);
        let edge = 6. + AVOID_GAP;
        for (x, y, w, h) in [
            (0, 0, 1280, 720),
            (0, 0, 600, 720),
            (300, 0, 500, 200),
            (600, 600, 600, 120),
            (100, 100, 60, 40),
            (1000, 300, 280, 200),
        ] {
            let selected = crop(x, y, w, h);
            let sel = PanelRect {
                x: x as f32,
                y: y as f32,
                width: w as f32,
                height: h as f32,
            };
            let toolbar = PanelRect {
                x: (sel.right() - 700.).max(12.),
                y: (sel.bottom() + 10.).min(720. - 60.),
                width: 700.,
                height: 48.,
            };
            let bar = layout(monitor, selected, 1., &[toolbar]);
            let outer = grow(sel, edge);
            let inner = grow(sel, -edge);
            assert!(
                !intersects(bar.panel, outer) || contains(inner, bar.panel),
                "{:?} 压在选区 {:?} 边框上",
                bar.panel,
                sel
            );
            inside(bar, monitor, 1.);
        }
    }

    #[test]
    fn selection_bar_stays_on_monitor_at_each_screen_edge_and_mixed_dpi() {
        let monitor = crop(1200, 100, 1400, 1000);
        for scale in [1., 1.25, 1.5, 2.] {
            for selected in [
                crop(1200, 100, 100, 100),
                crop(2500, 100, 100, 100),
                crop(1200, 1000, 100, 100),
                crop(2500, 1000, 100, 100),
            ] {
                let bar = layout(monitor, selected, scale, &[]);
                inside(bar, monitor, scale);
            }
        }
    }
}
