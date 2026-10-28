//! 贴图级联菜单的窗口与面板布局；所有面板坐标都相对菜单窗口。
use crate::pin_model::Placement;

pub(super) const ROW_HEIGHT: f32 = 26.;
pub(super) const PANEL_PADDING: f32 = 4.;
pub(super) const SEPARATOR_HEIGHT: f32 = 9.;
pub(super) const PANEL_WIDTH: f32 = 248.;
pub(super) const PANEL_RADIUS: f32 = 8.;
pub(super) const ROOT_ROWS: usize = 16;
const ROOT_SEPARATORS: usize = 3;

const ROOT_HEIGHT: f32 =
    ROOT_ROWS as f32 * ROW_HEIGHT + PANEL_PADDING * 2. + ROOT_SEPARATORS as f32 * SEPARATOR_HEIGHT;
// 最长的“显示”子菜单：18 行、3 条分组线。
const MAX_SUBMENU_HEIGHT: f32 = ROW_HEIGHT * 18. + SEPARATOR_HEIGHT * 3. + PANEL_PADDING * 2.;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Panel {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) width: f32,
    pub(super) height: f32,
}

impl Panel {
    pub(super) fn physical(self, scale: f32) -> Placement {
        // 两块面板共享同一个边界取整，避免高 DPI 下产生缝隙或交叠。
        let left = (self.x * scale).round() as i32;
        let top = (self.y * scale).round() as i32;
        let right = ((self.x + self.width) * scale).round() as i32;
        let bottom = ((self.y + self.height) * scale).round() as i32;
        Placement {
            x: left,
            y: top,
            width: (right - left).max(0) as u32,
            height: (bottom - top).max(0) as u32,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Layout {
    pub(super) placement: Placement,
    pub(super) root: Panel,
    pub(super) submenu_left: bool,
    pub(super) scale: f32,
}

impl Layout {
    pub(super) fn new(pointer: (i32, i32), work: Placement, scale: f32) -> Self {
        let scale = if scale.is_finite() && scale > 0. {
            scale
        } else {
            1.
        };
        let width = ((PANEL_WIDTH * 2. * scale).round() as u32)
            .max(1)
            .min(work.width.max(1));
        let height = ((ROOT_HEIGHT.max(MAX_SUBMENU_HEIGHT) * scale).round() as u32)
            .max(1)
            .min(work.height.max(1));
        let root_height = ((ROOT_HEIGHT * scale).round() as u32).max(1).min(height);
        let panel_width = width as f32 / scale / 2.;
        let root_left_offset = (panel_width * scale).round() as i64;
        let work_left = i64::from(work.x);
        let work_top = i64::from(work.y);
        let max_x = work_left + i64::from(work.width.max(1) - width);
        let max_y = work_top + i64::from(work.height.max(1) - height);
        let pointer_x = i64::from(pointer.0);

        let right_x = pointer_x.clamp(work_left, max_x);
        let left_x = (pointer_x - root_left_offset).clamp(work_left, max_x);
        // 右侧能容纳时优先向右；两侧都不足时选使主菜单最接近鼠标的一侧。
        let submenu_left =
            (left_x + root_left_offset - pointer_x).abs() < (right_x - pointer_x).abs();
        let x = if submenu_left { left_x } else { right_x };
        let root_top = i64::from(pointer.1).clamp(
            work_top,
            work_top + i64::from(work.height.max(1) - root_height),
        );
        let y = root_top.clamp(work_top, max_y);

        Self {
            placement: Placement {
                x: x as i32,
                y: y as i32,
                width,
                height,
            },
            root: Panel {
                x: if submenu_left { panel_width } else { 0. },
                y: (root_top - y) as f32 / scale,
                width: panel_width,
                height: root_height as f32 / scale,
            },
            submenu_left,
            scale,
        }
    }

    pub(super) fn submenu(&self, rows: usize, separators: usize, parent_y: f32) -> Panel {
        let window_height = self.placement.height as f32 / self.scale;
        let height =
            (rows as f32 * ROW_HEIGHT + separators as f32 * SEPARATOR_HEIGHT + PANEL_PADDING * 2.)
                .min(window_height);
        Panel {
            x: if self.submenu_left {
                0.
            } else {
                self.root.x + self.root.width
            },
            // 子菜单首行与父项同高，面板向上让出内边距。
            y: (parent_y - PANEL_PADDING).clamp(0., window_height - height),
            width: self.root.width,
            height,
        }
    }

    pub(super) fn root_row_y(&self, row: usize, separators_before: usize) -> f32 {
        self.root.y
            + PANEL_PADDING
            + row as f32 * ROW_HEIGHT
            + separators_before as f32 * SEPARATOR_HEIGHT
    }

    /// 圆角窗口区域的物理直径，与面板圆角一致。
    pub(super) fn corner_diameter(&self) -> i32 {
        (PANEL_RADIUS * 2. * self.scale).round() as i32
    }

    pub(super) fn regions(&self, active: Option<Panel>) -> Vec<Placement> {
        let mut regions = vec![self.root.physical(self.scale)];
        if let Some(submenu) = active {
            regions.push(submenu.physical(self.scale));
        }
        regions.retain(|region| region.width > 0 && region.height > 0);
        regions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(x: i32, y: i32, width: u32, height: u32) -> Placement {
        Placement {
            x,
            y,
            width,
            height,
        }
    }

    fn assert_inside(inner: Placement, outer: Placement) {
        assert!(inner.x >= outer.x && inner.y >= outer.y);
        assert!(
            i64::from(inner.x) + i64::from(inner.width)
                <= i64::from(outer.x) + i64::from(outer.width)
        );
        assert!(
            i64::from(inner.y) + i64::from(inner.height)
                <= i64::from(outer.y) + i64::from(outer.height)
        );
    }

    #[test]
    fn room_on_right_keeps_root_at_pointer() {
        let screen = work(0, 0, 1920, 1080);
        let layout = Layout::new((400, 300), screen, 1.);
        assert!(!layout.submenu_left);
        assert_eq!(layout.placement, work(400, 300, 496, 503));
        assert_eq!(layout.root.physical(layout.scale), work(0, 0, 248, 451));
        assert_inside(layout.placement, screen);
    }

    #[test]
    fn bottom_right_edge_opens_left_and_keeps_root_visible() {
        let screen = work(0, 0, 1920, 1080);
        let layout = Layout::new((1910, 1040), screen, 1.);
        assert!(layout.submenu_left);
        assert_inside(layout.placement, screen);
        let root = layout.root.physical(layout.scale);
        assert_eq!(root.x, 248);
        assert_eq!(layout.placement.y + root.y + root.height as i32, 1080);
        let submenu = layout.submenu(18, 3, layout.root_row_y(12, 2));
        assert_eq!(submenu.x + submenu.width, layout.root.x);
        assert_eq!(submenu.y, 0.);
    }

    #[test]
    fn negative_monitor_coordinates_remain_physical() {
        let screen = work(-1920, -240, 1920, 1080);
        let layout = Layout::new((-1750, -120), screen, 1.25);
        assert!(!layout.submenu_left);
        assert_eq!((layout.placement.x, layout.placement.y), (-1750, -120));
        assert_inside(layout.placement, screen);
    }

    #[test]
    fn fractional_dpi_regions_share_one_edge() {
        let screen = work(0, 0, 2560, 1440);
        let layout = Layout::new((400, 200), screen, 1.333);
        let regions = layout.regions(Some(layout.submenu(18, 3, layout.root_row_y(10, 1))));
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].x + regions[0].width as i32, regions[1].x);
        assert_eq!(
            regions[1].x + regions[1].width as i32,
            layout.placement.width as i32
        );
        for region in regions {
            assert_inside(
                region,
                work(0, 0, layout.placement.width, layout.placement.height),
            );
        }
        assert_inside(layout.placement, screen);
    }

    #[test]
    fn narrow_short_screen_clips_height_and_shrinks_both_panels() {
        let screen = work(-420, -180, 420, 220);
        let layout = Layout::new((-50, -10), screen, 1.5);
        assert_eq!(layout.placement, screen);
        assert_eq!(layout.root.width, 140.);
        let regions = layout.regions(Some(layout.submenu(18, 3, layout.root_row_y(14, 2))));
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].width, 210);
        assert_eq!(regions[1].width, 210);
        for region in regions {
            assert_inside(region, work(0, 0, 420, 220));
        }
    }

    #[test]
    fn submenu_tracks_parent_until_bottom_clamp_is_needed() {
        let layout = Layout::new((100, 100), work(0, 0, 1920, 1080), 1.);
        let parent_y = layout.root_row_y(1, 0);
        let submenu = layout.submenu(3, 0, parent_y);
        assert_eq!(submenu.y + PANEL_PADDING, parent_y);
        assert_eq!(submenu.x, layout.root.x + layout.root.width);
        let lower = layout.submenu(6, 1, layout.root_row_y(14, 2));
        assert_eq!(lower.y + lower.height, layout.placement.height as f32);
        assert_eq!(layout.regions(None).len(), 1);
    }
}
