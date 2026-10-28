/// 取色会话的显示格式与物理屏幕坐标；输入资格由宿主决定。
#[derive(Default)]
pub(crate) struct ColorDisplay {
    use_hex: bool,
    use_relative: bool,
    shift_was_down: bool,
    p_was_down: bool,
}

impl ColorDisplay {
    /// 进入或退出会话都恢复默认，并播种已经按住的键。
    pub(crate) fn reset(&mut self, shift_down: bool, p_down: bool) {
        *self = Self {
            shift_was_down: shift_down,
            p_was_down: p_down,
            ..Self::default()
        };
    }

    /// Shift 已由宿主合并 generic / 左 / 右状态；仅新的按下切换。
    /// 被输入或临时移动拒绝时仍消费边沿，恢复资格后不能补切。
    pub(crate) fn observe(&mut self, shift_down: bool, p_down: bool, eligible: bool) -> bool {
        let shift_pressed = shift_down && !self.shift_was_down;
        let p_pressed = p_down && !self.p_was_down;
        self.shift_was_down = shift_down;
        self.p_was_down = p_down;
        if !eligible {
            return false;
        }
        if shift_pressed {
            self.use_hex = !self.use_hex;
        }
        if p_pressed {
            self.use_relative = !self.use_relative;
        }
        shift_pressed || p_pressed
    }

    /// 输入已经是预乘 BGRA；格式化不再次乘 alpha。
    pub(crate) fn format_color(&self, bgra: [u8; 4]) -> String {
        let [b, g, r, _] = bgra;
        if self.use_hex {
            format!("#{r:02X}{g:02X}{b:02X}")
        } else {
            format!("rgb({r}, {g}, {b})")
        }
    }

    /// 相对坐标基于方向变化后的内容屏幕原点，不是原图或 crop 坐标。
    pub(crate) fn format_coords(
        &self,
        cursor: (i32, i32),
        oriented_host_origin: (i32, i32),
    ) -> String {
        let (mut x, mut y) = (i64::from(cursor.0), i64::from(cursor.1));
        if self.use_relative {
            x -= i64::from(oriented_host_origin.0);
            y -= i64::from(oriented_host_origin.1);
        }
        format!("( {x} , {y} )")
    }
}

#[cfg(test)]
mod tests {
    use super::ColorDisplay;

    #[test]
    fn reentry_restores_rgb_global_and_seeds_already_held_keys() {
        let mut display = ColorDisplay::default();
        assert!(display.observe(true, true, true));
        assert_eq!(display.format_color([86, 52, 18, 255]), "#123456");
        assert_eq!(display.format_coords((141, 128), (100, 80)), "( 41 , 48 )");

        display.reset(true, true);
        assert_eq!(display.format_color([86, 52, 18, 255]), "rgb(18, 52, 86)");
        assert_eq!(
            display.format_coords((141, 128), (100, 80)),
            "( 141 , 128 )"
        );
        assert!(!display.observe(true, true, true));
        assert!(!display.observe(false, false, true));
        assert!(display.observe(true, true, true));
        assert_eq!(display.format_color([86, 52, 18, 255]), "#123456");

        display.reset(false, false);
        assert_eq!(display.format_color([86, 52, 18, 255]), "rgb(18, 52, 86)");
        assert_eq!(
            display.format_coords((141, 128), (100, 80)),
            "( 141 , 128 )"
        );
        assert!(display.observe(false, true, true));
        assert_eq!(display.format_coords((141, 128), (100, 80)), "( 41 , 48 )");
    }

    #[test]
    fn held_repeat_and_overlapping_merged_shift_only_toggle_on_new_press() {
        let mut display = ColorDisplay::default();
        // 左 Shift 按下、重复、右 Shift 同时按下、左键松开：合并后持续为 true。
        for (shift_down, changed, expected) in [
            (false, false, "rgb(18, 52, 86)"),
            (true, true, "#123456"),
            (true, false, "#123456"),
            (true, false, "#123456"),
            (true, false, "#123456"),
            (false, false, "#123456"),
            (true, true, "rgb(18, 52, 86)"),
            (false, false, "rgb(18, 52, 86)"),
        ] {
            assert_eq!(display.observe(shift_down, false, true), changed);
            assert_eq!(display.format_color([86, 52, 18, 255]), expected);
        }
        assert!(display.observe(false, true, true));
        assert!(!display.observe(false, true, true));
        assert!(!display.observe(false, false, true));
        assert_eq!(display.format_coords((30, 40), (10, 10)), "( 20 , 30 )");
        assert!(display.observe(false, true, true));
        assert_eq!(display.format_coords((30, 40), (10, 10)), "( 30 , 40 )");
    }

    #[test]
    fn rejected_input_or_move_consumes_edges_without_delayed_toggle() {
        let mut display = ColorDisplay::default();
        assert!(!display.observe(true, true, false));
        assert!(!display.observe(true, true, true));
        assert_eq!(display.format_color([86, 52, 18, 255]), "rgb(18, 52, 86)");
        assert_eq!(
            display.format_coords((-141, 128), (-200, 80)),
            "( -141 , 128 )"
        );
        assert!(!display.observe(false, false, false));
        assert!(display.observe(true, false, true));
        assert_eq!(display.format_color([86, 52, 18, 255]), "#123456");

        // 临时移动期间 P 的一次按下与松开都被消费。
        assert!(!display.observe(true, true, false));
        assert!(!display.observe(false, false, false));
        assert!(!display.observe(false, false, true));
        assert_eq!(
            display.format_coords((-141, 128), (-200, 80)),
            "( -141 , 128 )"
        );
        assert!(display.observe(false, true, true));
        assert_eq!(
            display.format_coords((-141, 128), (-200, 80)),
            "( 59 , 48 )"
        );
        assert_eq!(display.format_color([86, 52, 18, 255]), "#123456");
    }

    #[test]
    fn color_formats_use_bgra_channels_and_do_not_premultiply_twice() {
        let mut display = ColorDisplay::default();
        assert_eq!(display.format_color([86, 52, 18, 255]), "rgb(18, 52, 86)");
        assert_eq!(display.format_color([45, 30, 15, 128]), "rgb(15, 30, 45)");
        assert_eq!(display.format_color([0, 0, 0, 0]), "rgb(0, 0, 0)");
        assert!(display.observe(true, false, true));
        assert_eq!(display.format_color([86, 52, 18, 255]), "#123456");
        assert_eq!(display.format_color([45, 30, 15, 128]), "#0F1E2D");
        assert_eq!(display.format_color([45, 30, 15, 255]), "#0F1E2D");
        assert_eq!(display.format_color([0, 0, 0, 0]), "#000000");
    }

    #[test]
    fn coordinates_keep_negative_multi_screen_physical_pixels_and_wide_differences() {
        let mut display = ColorDisplay::default();
        assert_eq!(
            display.format_coords((-1709, -751), (-1920, -1080)),
            "( -1709 , -751 )"
        );
        assert!(display.observe(false, true, true));
        assert_eq!(
            display.format_coords((-1709, -751), (-1920, -1080)),
            "( 211 , 329 )"
        );
        // 150% DPI / 1.5 倍显示下传入的是物理屏幕差值，不能除成原图的 80 / 60。
        assert_eq!(
            display.format_coords((-1680, 190), (-1800, 100)),
            "( 120 , 90 )"
        );
        assert_eq!(
            display.format_coords((i32::MAX, i32::MIN), (i32::MIN, i32::MAX)),
            "( 4294967295 , -4294967295 )"
        );
    }
}
