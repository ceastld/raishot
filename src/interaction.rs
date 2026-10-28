//! 与 UI 无关的滚轮参数和光标语义；高分辨率滚轮按完整刻度累积。
use crate::annotation::{Style, Tool};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parameter {
    Width,
    Font,
    Radius,
    Opacity,
    Block,
    Crop,
    TextOutline,
    TextBackgroundAlpha,
    TextBackgroundRadius,
}

/// 效果副栏只决定下一笔工具；沿用矩形/笔刷入口，不转换已提交对象。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrawingEffect {
    Mosaic,
    Blur,
    LocalFill,
}
impl DrawingEffect {
    pub(crate) fn of(tool: Tool) -> Option<Self> {
        match tool {
            Tool::Mosaic | Tool::MosaicBrush => Some(Self::Mosaic),
            Tool::Blur | Tool::BlurBrush => Some(Self::Blur),
            Tool::LocalFill => Some(Self::LocalFill),
            _ => None,
        }
    }
    pub(crate) fn next_tool(self, current: Tool, effect_brush_mode: bool) -> Tool {
        let brush = if current == Tool::LocalFill {
            effect_brush_mode
        } else {
            current.freehand()
        };
        match self {
            Self::Mosaic if brush => Tool::MosaicBrush,
            Self::Mosaic => Tool::Mosaic,
            Self::Blur if brush => Tool::BlurBrush,
            Self::Blur => Tool::Blur,
            Self::LocalFill => Tool::LocalFill,
        }
    }
}

impl Parameter {
    /// 数值入口与滚轮共用的绘制档位；透明度使用 UI 百分数，样式仍保存 0–1。
    pub fn limits(self, tool: Tool) -> Option<(f32, f32)> {
        Some(match self {
            Self::Width => match tool {
                Tool::Spotlight => (0., 20.),
                Tool::Highlighter => (4., 48.),
                Tool::Eraser => (12., 200.),
                Tool::PixelEraser => (4., 80.),
                Tool::LocalFill => (1., 50.),
                Tool::MosaicBrush | Tool::BlurBrush => (4., 200.),
                _ => (1., 50.),
            },
            Self::Font => (8., 72.),
            Self::Radius | Self::TextBackgroundRadius => (0., 50.),
            Self::Opacity | Self::TextBackgroundAlpha => (0., 100.),
            Self::Block if tool == Tool::LocalFill => return None,
            Self::Block => (2., 64.),
            Self::TextOutline => (1., 12.),
            Self::Crop => return None,
        })
    }
    pub fn for_tool(tool: Tool, shift: bool, control: bool) -> Option<Self> {
        if tool == Tool::SmartMosaic {
            return Some(Self::Block);
        }
        if tool == Tool::LocalFill {
            return Some(Self::Width);
        }
        if shift && tool.supports_radius() {
            return Some(Self::Radius);
        }
        if control {
            if tool == Tool::Spotlight {
                return Some(Self::Width);
            }
            return tool.effect().then_some(Self::Block);
        }
        Some(match tool {
            Tool::Crop => Self::Crop,
            Tool::Edit | Tool::Lasso => return None,
            Tool::Text | Tool::Watermark | Tool::Serial => Self::Font,
            Tool::Mosaic | Tool::Blur | Tool::SmartMosaic => Self::Block,
            Tool::Spotlight | Tool::HighlighterRect => Self::Opacity,
            _ => Self::Width,
        })
    }
    pub fn adjusted(self, mut style: Style, tool: Tool, steps: i32) -> Style {
        let Some((min, max)) = self.limits(tool) else {
            return style;
        };
        let d = steps as f32;
        match self {
            Self::Width => {
                style.width = if matches!(tool, Tool::Eraser | Tool::LocalFill) {
                    // 擦除/填充笔不通过对象缩放延伸绘制范围，保持自己的有效直径档位。
                    (style.width + d).clamp(min, max)
                } else {
                    bounded_step(style.width, d, min, max)
                };
            }
            Self::Font => style.font_size = bounded_step(style.font_size, d, min, max),
            Self::Radius => {
                let radius = bounded_step(style.radius_for(tool), d, min, max);
                style.set_radius_for(tool, radius);
            }
            Self::Opacity => {
                style.opacity = ((style.opacity * 100.).round() + d).clamp(min, max) / 100.
            }
            Self::Block => {
                style.block_size =
                    (style.block_size as i32 + steps).clamp(min as i32, max as i32) as u32
            }
            Self::TextOutline => {
                style.text.outline_width = bounded_step(style.text.outline_width, d, min, max)
            }
            Self::TextBackgroundAlpha => {
                style.text.background_opacity =
                    ((style.text.background_opacity * 100.).round() + d).clamp(min, max) / 100.
            }
            Self::TextBackgroundRadius => {
                style.text.background_radius =
                    bounded_step(style.text.background_radius, d, min, max)
            }
            Self::Crop => {}
        }
        style
    }
}
// 缩放可产生超出绘制档位的值；滚轮不能逆向跳变，回调方向仍按原步长调整。
fn bounded_step(value: f32, delta: f32, min: f32, max: f32) -> f32 {
    (value + delta).clamp(min.min(value), max.max(value))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hover {
    Arrow,
    Crosshair,
    Text,
    Move,
    Horizontal,
    Vertical,
    DiagonalDown,
    DiagonalUp,
}
impl Hover {
    pub fn handle(index: usize) -> Self {
        match index {
            0 | 7 => Self::DiagonalDown,
            2 | 5 => Self::DiagonalUp,
            1 | 6 => Self::Vertical,
            _ => Self::Horizontal,
        }
    }
    pub fn edges(edges: u8) -> Self {
        match edges {
            5 | 10 => Self::DiagonalDown,
            6 | 9 => Self::DiagonalUp,
            1 | 2 => Self::Horizontal,
            4 | 8 => Self::Vertical,
            _ => Self::Crosshair,
        }
    }
}
#[derive(Clone, Debug, Default)]
pub struct WheelAccumulator {
    target: Option<WheelTarget>,
    last: Option<Instant>,
    remainder: f32,
    group: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WheelTarget {
    Parameter(Parameter),
    SelectionCenter,
}
impl WheelAccumulator {
    pub fn reset(&mut self) {
        self.last = None;
        self.remainder = 0.;
    }
    pub(crate) fn reset_selection_center(&mut self) {
        if self.target == Some(WheelTarget::SelectionCenter) {
            self.reset();
        }
    }
    pub fn push(&mut self, target: Parameter, notches: f32, now: Instant) -> (i32, u64) {
        self.push_target(WheelTarget::Parameter(target), notches, now)
    }
    pub(crate) fn push_selection_center(&mut self, notches: f32, now: Instant) -> (i32, u64) {
        self.push_target(WheelTarget::SelectionCenter, notches, now)
    }
    fn push_target(&mut self, target: WheelTarget, notches: f32, now: Instant) -> (i32, u64) {
        if !notches.is_finite() {
            return (0, self.group);
        }
        // 零刻度也声明了消费路线；参数已到限或横滚不能把旧中心半刻度带回来。
        if self.target != Some(target) {
            self.reset();
            self.target = Some(target);
        }
        if notches == 0. {
            return (0, self.group);
        }
        if self.target != Some(target)
            || self
                .last
                .is_none_or(|last| now.duration_since(last) > Duration::from_millis(450))
        {
            self.group = self.group.wrapping_add(1);
            self.remainder = 0.;
        }
        if self.remainder.signum() != notches.signum() {
            self.remainder = 0.;
        }
        self.target = Some(target);
        self.last = Some(now);
        self.remainder += notches.clamp(-32., 32.);
        let steps = (self.remainder.abs() + 0.0001).floor() as i32
            * if self.remainder < 0. { -1 } else { 1 };
        self.remainder -= steps as f32;
        (steps, self.group)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_center_does_not_share_partial_notches_with_legacy_crop_or_parameters() {
        let now = Instant::now();
        let mut wheel = WheelAccumulator::default();
        // 无 crop 的 UIA 尝试未消费时只退出中心；原参数 fallback 的半刻度仍有效。
        assert_eq!(wheel.push(Parameter::Width, 0.5, now).0, 0);
        wheel.reset_selection_center();
        assert_eq!(wheel.push(Parameter::Width, 0.5, now).0, 1);
        assert_eq!(wheel.push_selection_center(0.5, now).0, 0);
        wheel.reset_selection_center();
        assert_eq!(wheel.push_selection_center(0.5, now).0, 0);
        wheel.reset();
        assert_eq!(wheel.push_selection_center(0.5, now).0, 0);
        assert_eq!(wheel.push(Parameter::Crop, 0.5, now).0, 0);
        assert_eq!(wheel.push_selection_center(0.5, now).0, 0);
        assert_eq!(wheel.push(Parameter::Width, 0., now).0, 0);
        assert_eq!(wheel.push_selection_center(0.5, now).0, 0);
        assert_eq!(
            wheel
                .push_selection_center(0.5, now + Duration::from_millis(20))
                .0,
            1
        );
    }
    #[test]
    fn selection_center_clone_preserves_burst_direction_timeout_and_reset_boundaries() {
        let now = Instant::now();
        let mut native = WheelAccumulator::default();
        assert_eq!(native.push_selection_center(0.5, now).0, 0);
        let mut gpui = native.clone();
        assert_eq!(
            gpui.push_selection_center(0.5, now + Duration::from_millis(20))
                .0,
            1
        );
        assert_eq!(
            gpui.push_selection_center(-0.5, now + Duration::from_millis(30))
                .0,
            0
        );
        assert_eq!(
            gpui.push_selection_center(0.5, now + Duration::from_millis(40))
                .0,
            0
        );
        assert_eq!(
            gpui.push_selection_center(0.5, now + Duration::from_millis(500))
                .0,
            0
        );
        gpui.reset();
        assert_eq!(
            gpui.push_selection_center(0.5, now + Duration::from_millis(520))
                .0,
            0
        );
    }
    #[test]
    fn local_fill_wheel_uses_one_pixel_width_step_with_all_modifiers_and_strict_bounds() {
        let style = crate::tool_presets::default_style(Tool::LocalFill);
        assert_eq!(style.width, 10.);
        assert_eq!(Parameter::Width.limits(Tool::LocalFill), Some((1., 50.)));
        assert_eq!(Parameter::Block.limits(Tool::LocalFill), None);
        for shift in [false, true] {
            for control in [false, true] {
                assert_eq!(
                    Parameter::for_tool(Tool::LocalFill, shift, control),
                    Some(Parameter::Width)
                );
            }
        }
        let up = Parameter::Width.adjusted(style, Tool::LocalFill, 1);
        let down = Parameter::Width.adjusted(style, Tool::LocalFill, -1);
        assert_eq!((up.width, down.width), (11., 9.));
        assert_eq!(up.block_size, style.block_size);
        assert_eq!(up.opacity, style.opacity);
        assert_eq!(
            Parameter::Width
                .adjusted(style, Tool::LocalFill, -1000)
                .width,
            1.
        );
        assert_eq!(
            Parameter::Width
                .adjusted(style, Tool::LocalFill, 1000)
                .width,
            50.
        );
        assert_eq!(Parameter::Block.adjusted(style, Tool::LocalFill, 5), style);
    }
    #[test]
    fn local_fill_effect_choice_keeps_existing_rectangles_and_selects_only_next_tool() {
        use crate::{
            annotation::{Annotation, Point},
            editor::Editor,
            geometry::Rect,
        };
        let area = Rect::full(320, 240);
        let mut editor = Editor::new(area, Some(area));
        let mut annotation =
            Annotation::new(Tool::Mosaic, Point { x: 20., y: 30. }, Style::default());
        annotation.update(Point { x: 90., y: 80. });
        let id = editor.document.insert(annotation);
        editor.select_tool(Tool::Edit);
        editor.selected = Some(id);
        editor.selection = vec![id];
        let before = editor.document.items().to_vec();
        let revision = editor.document.revision();
        let next = DrawingEffect::LocalFill.next_tool(Tool::Mosaic, editor.effect_brush_mode);
        editor.select_tool(next);
        assert_eq!(editor.tool, Tool::LocalFill);
        assert_eq!(editor.style.width, 10.);
        assert_eq!(editor.document.items(), before);
        assert_eq!(editor.document.revision(), revision);
        assert!(editor.selected_ids().is_empty());
        assert_eq!(
            DrawingEffect::Blur.next_tool(Tool::Mosaic, true),
            Tool::Blur
        );
        assert_eq!(
            DrawingEffect::Mosaic.next_tool(Tool::Blur, true),
            Tool::Mosaic
        );
        assert_eq!(
            DrawingEffect::Mosaic.next_tool(Tool::LocalFill, false),
            Tool::Mosaic
        );
        assert_eq!(
            DrawingEffect::Blur.next_tool(Tool::LocalFill, false),
            Tool::Blur
        );
        editor.select_tool(Tool::MosaicBrush);
        editor.select_tool(Tool::LocalFill);
        assert!(editor.effect_brush_mode);
        assert_eq!(
            DrawingEffect::Mosaic.next_tool(Tool::LocalFill, editor.effect_brush_mode),
            Tool::MosaicBrush
        );
        assert_eq!(
            DrawingEffect::Blur.next_tool(Tool::LocalFill, editor.effect_brush_mode),
            Tool::BlurBrush
        );
        assert_eq!(
            DrawingEffect::of(Tool::LocalFill),
            Some(DrawingEffect::LocalFill)
        );
        assert_eq!(DrawingEffect::of(Tool::PixelEraser), None);
    }
    #[test]
    fn numeric_control_limits_match_wheel_endpoints_in_display_units() {
        fn value(parameter: Parameter, style: Style) -> f32 {
            match parameter {
                Parameter::Width => style.width,
                Parameter::Font => style.font_size,
                Parameter::Radius => style.radius,
                Parameter::Opacity => style.opacity * 100.,
                Parameter::Block => style.block_size as f32,
                Parameter::TextOutline => style.text.outline_width,
                Parameter::TextBackgroundAlpha => style.text.background_opacity * 100.,
                Parameter::TextBackgroundRadius => style.text.background_radius,
                Parameter::Crop => unreachable!(),
            }
        }
        for (parameter, tool, expected) in [
            (Parameter::Width, Tool::Rectangle, (1., 50.)),
            (Parameter::Width, Tool::Spotlight, (0., 20.)),
            (Parameter::Width, Tool::Highlighter, (4., 48.)),
            (Parameter::Width, Tool::Eraser, (12., 200.)),
            (Parameter::Width, Tool::PixelEraser, (4., 80.)),
            (Parameter::Width, Tool::MosaicBrush, (4., 200.)),
            (Parameter::Width, Tool::BlurBrush, (4., 200.)),
            (Parameter::Font, Tool::Text, (8., 72.)),
            (Parameter::Radius, Tool::Rectangle, (0., 50.)),
            (Parameter::Opacity, Tool::Spotlight, (0., 100.)),
            (Parameter::Block, Tool::Mosaic, (2., 64.)),
            (Parameter::TextOutline, Tool::Text, (1., 12.)),
            (Parameter::TextBackgroundAlpha, Tool::Text, (0., 100.)),
            (Parameter::TextBackgroundRadius, Tool::Text, (0., 50.)),
        ] {
            assert_eq!(parameter.limits(tool), Some(expected));
            let middle = (expected.0 + expected.1) / 2.;
            let mut style = Style::default();
            match parameter {
                Parameter::Width => style.width = middle,
                Parameter::Font => style.font_size = middle,
                Parameter::Radius => style.radius = middle,
                Parameter::Opacity => style.opacity = middle / 100.,
                Parameter::Block => style.block_size = middle as u32,
                Parameter::TextOutline => style.text.outline_width = middle,
                Parameter::TextBackgroundAlpha => style.text.background_opacity = middle / 100.,
                Parameter::TextBackgroundRadius => style.text.background_radius = middle,
                Parameter::Crop => unreachable!(),
            }
            assert_eq!(
                value(parameter, parameter.adjusted(style, tool, -1000)),
                expected.0
            );
            assert_eq!(
                value(parameter, parameter.adjusted(style, tool, 1000)),
                expected.1
            );
        }
        assert_eq!(Parameter::Crop.limits(Tool::Crop), None);
        assert_eq!(
            Parameter::Crop.adjusted(Style::default(), Tool::Crop, 1000),
            Style::default()
        );
    }
    #[test]
    fn precise_wheel_accumulates_and_separates_direction_target_and_burst() {
        let mut w = WheelAccumulator::default();
        let t = Instant::now();
        assert_eq!(w.push(Parameter::Width, 0.4, t).0, 0);
        let (steps, group) = w.push(Parameter::Width, 0.6, t);
        assert_eq!(steps, 1);
        assert_eq!(w.push(Parameter::Width, 3., t), (3, group));
        assert_eq!(w.push(Parameter::Width, -0.6, t).0, 0);
        assert_eq!(w.push(Parameter::Radius, -0.4, t).0, 0);
        assert_eq!(w.push(Parameter::Radius, -0.6, t).0, -1);
        assert_ne!(
            w.push(Parameter::Radius, 1., t + Duration::from_secs(1)).1,
            group
        );
        w.reset();
        assert_eq!(w.push(Parameter::Radius, 0.8, t).0, 0);
    }
    #[test]
    fn tool_wheel_mapping_and_limits_are_explicit() {
        assert_eq!(
            Parameter::for_tool(Tool::Rectangle, true, false),
            Some(Parameter::Radius)
        );
        assert_eq!(
            Parameter::for_tool(Tool::MosaicBrush, false, false),
            Some(Parameter::Width)
        );
        assert_eq!(
            Parameter::for_tool(Tool::MosaicBrush, false, true),
            Some(Parameter::Block)
        );
        let s = Style::default();
        assert_eq!(
            Parameter::Width.adjusted(s, Tool::Rectangle, 500).width,
            50.
        );
        assert_eq!(Parameter::Font.adjusted(s, Tool::Text, -500).font_size, 8.);
        assert_eq!(Parameter::Width.adjusted(s, Tool::Eraser, -500).width, 12.);
        assert_eq!(
            Parameter::Opacity
                .adjusted(s, Tool::Spotlight, -500)
                .opacity,
            0.
        );
    }
    #[test]
    fn highlight_radius_wheel_keeps_default_opacity_and_shared_limits() {
        let style = Style {
            radius: 17.5,
            opacity: 0.35,
            ..Style::default()
        };
        for tool in [Tool::Rectangle, Tool::Spotlight, Tool::HighlighterRect] {
            assert_eq!(
                Parameter::for_tool(tool, true, false),
                Some(Parameter::Radius)
            );
            assert_eq!(
                Parameter::for_tool(tool, true, true),
                Some(Parameter::Radius)
            );
            assert_eq!(Parameter::Radius.limits(tool), Some((0., 50.)));
            let changed = Parameter::Radius.adjusted(style, tool, 2);
            assert_eq!(changed.radius, 19.5);
            assert_eq!(changed.opacity, style.opacity);
        }
        assert_eq!(
            Parameter::for_tool(Tool::HighlighterRect, false, false),
            Some(Parameter::Opacity)
        );
        assert_eq!(
            Parameter::for_tool(Tool::Highlighter, true, false),
            Some(Parameter::Width)
        );
    }
    #[test]
    fn scaled_values_outside_drawing_limits_scroll_monotonically_without_jump() {
        let style = Style {
            width: 80.,
            font_size: 112.,
            radius: 65.,
            ..Style::default()
        };
        assert_eq!(Parameter::Width.adjusted(style, Tool::Rectangle, 1), style);
        assert_eq!(Parameter::Font.adjusted(style, Tool::Text, 1), style);
        assert_eq!(Parameter::Radius.adjusted(style, Tool::Rectangle, 1), style);
        assert_eq!(
            Parameter::Width.adjusted(style, Tool::Rectangle, -1).width,
            79.
        );
        assert_eq!(
            Parameter::Font.adjusted(style, Tool::Text, -1).font_size,
            111.
        );
        assert_eq!(
            Parameter::Radius
                .adjusted(style, Tool::Rectangle, -1)
                .radius,
            64.
        );
        let small = Style {
            width: 0.5,
            font_size: 4.,
            ..style
        };
        assert_eq!(Parameter::Width.adjusted(small, Tool::Rectangle, -1), small);
        assert_eq!(Parameter::Font.adjusted(small, Tool::Text, -1), small);
        assert_eq!(
            Parameter::Width.adjusted(small, Tool::Rectangle, 1).width,
            1.5
        );
        assert_eq!(Parameter::Font.adjusted(small, Tool::Text, 1).font_size, 5.);
    }
}
