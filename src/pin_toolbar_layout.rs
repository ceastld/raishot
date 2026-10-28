//! 贴图两级栏的纯布局：一级靠近图片并与其右缘对齐，二级始终在整组外侧。
//! 不依赖 HWND；先为所有工具预留二级高度，再决定整组上下位置。
use crate::{
    annotation::Tool, geometry::Rect, pin_model::Placement, toolbar_layout::ToolbarLayout,
};

/// 默认 7 个绘制工具（含局部橡皮擦）连同固定按钮恰好一行。
const PRIMARY_WIDTH: f32 = 546.;
const PRIMARY_HEIGHT: f32 = 48.;
const GAP: f32 = 8.;
const TOOLS: [Tool; 24] = [
    Tool::Crop,
    Tool::Edit,
    Tool::Rectangle,
    Tool::Ellipse,
    Tool::Triangle,
    Tool::Diamond,
    Tool::Star,
    Tool::Arrow,
    Tool::Pen,
    Tool::Mosaic,
    Tool::Highlighter,
    Tool::HighlighterRect,
    Tool::Polyline,
    Tool::Text,
    Tool::Watermark,
    Tool::Serial,
    Tool::Blur,
    Tool::MosaicBrush,
    Tool::BlurBrush,
    Tool::LocalFill,
    Tool::Spotlight,
    Tool::Eraser,
    Tool::PixelEraser,
    Tool::Lasso,
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PinToolbarLayout {
    pub primary: Placement,
    pub properties_above: bool,
}
impl PinToolbarLayout {
    pub fn new(pin: Placement, work: Placement, dpi: f32) -> Self {
        // 旧入口保留固定单行 546×48 DIP，与默认工具数的配置布局一致。
        Self::place(pin, work, dpi, PRIMARY_WIDTH, PRIMARY_HEIGHT)
    }

    /// 移动/Edit、更多、历史和输出固定可达；超过一行时增加一级高度。
    pub fn new_with_tool_count(pin: Placement, work: Placement, dpi: f32, count: usize) -> Self {
        let dpi = scale(dpi);
        let items = primary_items(count.min(crate::toolbar_config::AVAILABLE_TOOLS.len()));
        let natural = items.iter().sum::<f32>() + items.len().saturating_sub(1) as f32 * 2. + 20.;
        let logical_width = natural.min(PRIMARY_WIDTH);
        let physical_width = pixels(logical_width, dpi).min(work.width);
        // 宿主边框 + p(7) 共占 16 DIP；额外 4 DIP 是默认栏原有余量。
        let inner = (physical_width as f32 / dpi - 16.).max(32.);
        let (mut rows, mut used) = (1_f32, 0_f32);
        for item in items {
            let next = if used == 0. { item } else { used + 2. + item };
            if next > inner && used > 0. {
                rows += 1.;
                used = item;
            } else {
                used = next;
            }
        }
        let logical_height = rows * 32. + (rows - 1.) * 2. + 16.;
        Self::place(pin, work, dpi, logical_width, logical_height)
    }

    fn place(
        pin: Placement,
        work: Placement,
        dpi: f32,
        logical_width: f32,
        logical_height: f32,
    ) -> Self {
        let dpi = scale(dpi);
        let width = pixels(logical_width, dpi).min(work.width);
        // 极矮工作区仍给二级至少留一个像素；一级高度由实际按钮行数决定。
        let height = pixels(logical_height, dpi).min(if work.height > 1 {
            work.height - 1
        } else {
            work.height
        });
        let spare = work.height.saturating_sub(height);
        let gap = pixels(GAP, dpi).min(spare.saturating_sub(1));
        let reserve = TOOLS
            .into_iter()
            .flat_map(|tool| {
                [1, 2]
                    .map(|count| pixels(Self::property_size(work, dpi, tool, count, false).1, dpi))
            })
            .max()
            .unwrap_or(0)
            .min(spare.saturating_sub(gap));
        let group_height = height as i64 + gap as i64 + reserve as i64;
        let outer_gap = pixels(GAP, dpi) as i64;
        let work_top = work.y as i64;
        let work_bottom = work_top + work.height as i64;
        let below = pin.y as i64 + pin.height as i64 + outer_gap;
        let below_room = (work_bottom - below.max(work_top)).max(0);
        let above_room = ((pin.y as i64 - outer_gap).min(work_bottom) - work_top).max(0);
        let fits_below = below_room >= group_height;
        let fits_above = above_room >= group_height;
        let properties_above = !fits_below && (fits_above || above_room >= below_room);
        let minimum_secondary = i64::from(spare > 0);
        // 图片占满工作区、连一级都无处避让时，保留两栏完整可用空间；
        // 不能为了靠近屏边把二级缩成一像素。只能容纳部分二级时才缩 viewport。
        let above_reserve = if fits_above || above_room < height as i64 + minimum_secondary {
            reserve as i64 + gap as i64
        } else {
            minimum_secondary
        };
        let below_height = if fits_below || below_room < height as i64 + minimum_secondary {
            group_height
        } else {
            height as i64 + minimum_secondary
        };
        let (desired_y, min_y, max_y) = if properties_above {
            (
                pin.y as i64 - outer_gap - height as i64,
                work_top + above_reserve,
                work_bottom - height as i64,
            )
        } else {
            (below, work_top, work_bottom - below_height)
        };
        Self {
            primary: Placement {
                x: right_aligned_x(pin, width, work),
                y: coordinate(desired_y.clamp(min_y, max_y.max(min_y))),
                width,
                height,
            },
            properties_above,
        }
    }

    /// 复用截图的同一套工具尺寸与变换行计算，不维护第二份参数控件宽度表。
    pub fn property_size(
        work: Placement,
        dpi: f32,
        tool: Tool,
        selected_count: usize,
        text_draft: bool,
    ) -> (f32, f32) {
        let (width, height, _) = Self::property_layout(work, dpi, tool, selected_count, text_draft);
        (width, height)
    }

    /// 贴图二级栏（宽，高，收起级别）；与截图二级栏共用同一份估算。
    pub fn property_layout(
        work: Placement,
        dpi: f32,
        tool: Tool,
        selected_count: usize,
        text_draft: bool,
    ) -> (f32, f32, u8) {
        let dpi = scale(dpi);
        let monitor = Rect::full(work.width.max(1), work.height.max(1));
        let mut layout = ToolbarLayout::new(
            monitor,
            Rect::full(monitor.width, 1),
            dpi,
            Some((12., 12.)),
            tool,
        );
        if !text_draft {
            layout = layout.with_transform_context(selected_count);
        }
        let width = if layout.properties.width > 0. {
            layout.properties.width
        } else {
            440_f32.min((work.width as f32 / dpi - 24.).max(64.))
        };
        (width, layout.properties.height, layout.property_level())
    }

    /// 二级与一级右缘对齐，只在预先选择的外侧放置；工具切换不会翻到另一侧。
    /// 高度不足时缩小 viewport，宿主用滚动容器承载完整内容。
    pub fn properties(
        primary: Placement,
        work: Placement,
        dpi: f32,
        logical_size: (f32, f32),
        above: bool,
    ) -> Placement {
        let dpi = scale(dpi);
        let width = pixels(logical_size.0, dpi).min(work.width);
        let top = work.y as i64;
        let bottom = top + work.height as i64;
        let primary_top = primary.y as i64;
        let primary_bottom = primary_top + primary.height as i64;
        let available = if above {
            (primary_top - top).max(0)
        } else {
            (bottom - primary_bottom).max(0)
        }
        .min(work.height as i64) as u32;
        let gap = pixels(GAP, dpi).min(available.saturating_sub(1));
        let height = pixels(logical_size.1, dpi).min(available.saturating_sub(gap));
        // 只有单像素/空工作区无法放下两个非零窗口，保持有界而不下溢。
        let height = if height == 0 && work.height > 0 {
            1
        } else {
            height
        };
        let desired_y = if above {
            primary_top - gap as i64 - height as i64
        } else {
            primary_bottom + gap as i64
        };
        Placement {
            x: right_aligned_x(primary, width, work),
            y: coordinate(desired_y.clamp(top, (bottom - height as i64).max(top))),
            width,
            height,
        }
    }
}

fn primary_items(count: usize) -> Vec<f32> {
    let mut items = Vec::with_capacity(count + 10);
    items.extend(std::iter::repeat_n(32., count + 3)); // 移动/Edit + 绘制工具 + 更多
    items.extend([7., 32., 32., 7., 32., 32., 32.]); // 两条分隔线、撤销/重做、复制/保存/完成
    items
}

fn scale(dpi: f32) -> f32 {
    if dpi.is_finite() && dpi > 1e-6 {
        dpi
    } else {
        1.
    }
}
fn pixels(logical: f32, dpi: f32) -> u32 {
    if !logical.is_finite() || logical <= 0. {
        return 0;
    }
    (logical as f64 * dpi as f64).ceil() as u32
}
fn coordinate(value: i64) -> i32 {
    value.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}
/// 与截图工具栏一致贴右下；比父窗宽时向左延伸，最终夹回工作区。
fn right_aligned_x(parent: Placement, width: u32, work: Placement) -> i32 {
    let desired = parent.x as i64 + parent.width as i64 - width as i64;
    let left = work.x as i64;
    let right = left + work.width.saturating_sub(width) as i64;
    coordinate(desired.clamp(left, right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configurable_tool_count_preserves_default_and_wraps_all_tools() {
        let pin = pin(250, 300);
        let legacy = PinToolbarLayout::new(pin, work(), 1.);
        let defaults = PinToolbarLayout::new_with_tool_count(
            pin,
            work(),
            1.,
            crate::toolbar_config::DEFAULT_TOOLS.len(),
        );
        assert_eq!(legacy, defaults);
        let empty = PinToolbarLayout::new_with_tool_count(pin, work(), 1., 0);
        assert_eq!((empty.primary.width, empty.primary.height), (308, 48));
        let all = PinToolbarLayout::new_with_tool_count(
            pin,
            work(),
            1.,
            crate::toolbar_config::AVAILABLE_TOOLS.len(),
        );
        assert_eq!((all.primary.width, all.primary.height), (546, 82));
        assert_eq!(
            all,
            PinToolbarLayout::new_with_tool_count(pin, work(), 1., usize::MAX)
        );
        let secondary = properties(all, work(), 1., Tool::Text, 1);
        assert!(contains(work(), all.primary) && contains(work(), secondary));
        assert!(!overlap(all.primary, secondary));
    }

    #[test]
    fn configurable_primary_wraps_on_narrow_high_dpi_work_without_reversing_the_group() {
        let work = Placement {
            x: -600,
            y: 20,
            width: 420,
            height: 1100,
        };
        let pin = Placement {
            x: -550,
            y: 920,
            width: 300,
            height: 160,
        };
        for dpi in [1., 1.25, 2.] {
            for count in [0, 6, 18] {
                let layout = PinToolbarLayout::new_with_tool_count(pin, work, dpi, count);
                assert!(layout.properties_above);
                assert!(contains(work, layout.primary));
                for tool in [Tool::Pen, Tool::Text, Tool::Polyline] {
                    let secondary = properties(layout, work, dpi, tool, 1);
                    assert!(contains(work, secondary));
                    assert!(!overlap(layout.primary, secondary));
                    assert!(secondary.y + secondary.height as i32 <= layout.primary.y);
                }
            }
        }
    }

    fn work() -> Placement {
        Placement {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }
    }
    fn pin(y: i32, height: u32) -> Placement {
        Placement {
            x: 700,
            y,
            width: 500,
            height,
        }
    }
    fn contains(work: Placement, child: Placement) -> bool {
        child.x as i64 >= work.x as i64
            && child.y as i64 >= work.y as i64
            && child.x as i64 + child.width as i64 <= work.x as i64 + work.width as i64
            && child.y as i64 + child.height as i64 <= work.y as i64 + work.height as i64
    }
    fn overlap(a: Placement, b: Placement) -> bool {
        (a.x as i64) < b.x as i64 + b.width as i64
            && (b.x as i64) < a.x as i64 + a.width as i64
            && (a.y as i64) < b.y as i64 + b.height as i64
            && (b.y as i64) < a.y as i64 + a.height as i64
    }
    fn properties(
        layout: PinToolbarLayout,
        work: Placement,
        dpi: f32,
        tool: Tool,
        count: usize,
    ) -> Placement {
        PinToolbarLayout::properties(
            layout.primary,
            work,
            dpi,
            PinToolbarLayout::property_size(work, dpi, tool, count, false),
            layout.properties_above,
        )
    }

    #[test]
    fn radius_bulk_ui_pin_reserves_multi_row_at_negative_origin_without_overlapping_primary() {
        let work = Placement {
            x: -1920,
            y: -1440,
            width: 1920,
            height: 1440,
        };
        let pin = Placement {
            x: -1600,
            y: -1200,
            width: 400,
            height: 100,
        };
        for dpi in [1., 2., 3.] {
            let layout = PinToolbarLayout::new(pin, work, dpi);
            assert!(!layout.properties_above);
            assert!(contains(work, layout.primary));
            assert!(!overlap(pin, layout.primary));
            let mut size = None;
            for tool in [Tool::Rectangle, Tool::Arrow, Tool::Edit] {
                let logical = PinToolbarLayout::property_size(work, dpi, tool, 3, false);
                // 多选的圆角入口与对象段同排，宽度够时只占一行。
                assert!(logical.1 >= crate::property_items::bar_height(1));
                if let Some(previous) = size {
                    assert_eq!(logical, previous);
                }
                size = Some(logical);
                let secondary = properties(layout, work, dpi, tool, 3);
                assert!(contains(work, secondary));
                assert!(!overlap(layout.primary, secondary));
                assert!(!overlap(pin, secondary));
            }
        }
    }
    #[test]
    fn enough_room_places_primary_below_pin_and_properties_below_primary() {
        let pin = pin(250, 300);
        let layout = PinToolbarLayout::new(pin, work(), 1.);
        let secondary = properties(layout, work(), 1., Tool::Text, 1);
        assert_eq!((layout.primary.width, layout.primary.height), (546, 48));
        assert!(!layout.properties_above);
        assert_eq!(layout.primary.y, 558);
        // 贴右下：一级、二级右缘都与图片右缘对齐。
        let right = |p: Placement| p.x + p.width as i32;
        assert_eq!(right(layout.primary), right(pin));
        assert_eq!(right(secondary), right(pin));
        assert_eq!(
            secondary.y,
            layout.primary.y + layout.primary.height as i32 + 8
        );
        assert!(contains(work(), secondary));
        assert!(!overlap(pin, layout.primary) && !overlap(pin, secondary));
    }

    #[test]
    fn bottom_pin_uses_entire_stack_above_and_primary_stays_nearest_pin() {
        let pin = pin(850, 170);
        let layout = PinToolbarLayout::new(pin, work(), 1.);
        assert!(
            layout.properties_above,
            "pin={pin:?}, layout={layout:?}, rectangle={:?}, polyline={:?}",
            PinToolbarLayout::property_size(work(), 1., Tool::Rectangle, 1, false),
            PinToolbarLayout::property_size(work(), 1., Tool::Polyline, 1, false)
        );
        assert_eq!(layout.primary.y + layout.primary.height as i32, pin.y - 8);
        for tool in TOOLS {
            let secondary = properties(layout, work(), 1., tool, 1);
            assert_eq!(secondary.y + secondary.height as i32, layout.primary.y - 8);
            assert!(contains(work(), secondary));
            assert!(!overlap(layout.primary, secondary));
            assert!(!overlap(pin, secondary));
        }
    }

    #[test]
    fn reserves_widest_tool_transform_before_selecting_side() {
        // 只放一级时底部仍有空间，但两级放不下，不能把二级单独翻进图片。
        let pin = pin(900, 90);
        let layout = PinToolbarLayout::new(pin, work(), 1.);
        assert!(
            layout.properties_above,
            "pin={pin:?}, layout={layout:?}, rectangle={:?}, polyline={:?}",
            PinToolbarLayout::property_size(work(), 1., Tool::Rectangle, 1, false),
            PinToolbarLayout::property_size(work(), 1., Tool::Polyline, 1, false)
        );
        let primary = layout.primary;
        for tool in TOOLS {
            for count in [0, 1, 2] {
                let secondary = properties(layout, work(), 1., tool, count);
                assert_eq!(layout.primary, primary);
                assert!(contains(work(), secondary));
                assert!(!overlap(primary, secondary));
                assert!(!overlap(pin, secondary));
            }
        }
    }

    #[test]
    fn physical_coordinates_support_negative_origin_and_fractional_dpi() {
        let work = Placement {
            x: -1920,
            y: -300,
            width: 1920,
            height: 1080,
        };
        let pin = Placement {
            x: -1740,
            y: -140,
            width: 320,
            height: 240,
        };
        let layout = PinToolbarLayout::new(pin, work, 1.5);
        assert_eq!((layout.primary.width, layout.primary.height), (819, 72));
        assert_eq!(layout.primary.x, work.x);
        for tool in TOOLS {
            let secondary = properties(layout, work, 1.5, tool, 1);
            assert!(contains(work, layout.primary) && contains(work, secondary));
            assert!(!overlap(layout.primary, secondary));
        }
    }

    #[test]
    fn property_size_reuses_shared_tool_layout_and_omits_transform_during_text_draft() {
        let work = work();
        use crate::property_items::{Hint, Item, GROUP_SEPARATOR, ITEM_GAP};
        let rectangle = PinToolbarLayout::property_size(work, 1., Tool::Rectangle, 0, false);
        // 色块 234 + 粗细 65 + 不透明度 80 ｜ 描边/填充 58 + 线型 58 + 圆角 72，加内边距 10。
        assert_eq!(rectangle, (610., 42.));
        // 选中对象时对象段（角度、排列、删除）追加在同一排，放得下就不加行。
        assert_eq!(
            PinToolbarLayout::property_size(work, 1., Tool::Rectangle, 1, false),
            (786., 42.)
        );
        assert_eq!(
            PinToolbarLayout::property_size(work, 1., Tool::Rectangle, 1, true),
            rectangle
        );
        // 箭头：端点 + 头型两组分段与线型。
        assert_eq!(
            PinToolbarLayout::property_size(work, 1., Tool::Arrow, 0, false),
            (652., 42.)
        );
        // 多选：计数｜圆角入口与适用提示｜对象段，同排。
        let multi = Item::Count(2).width()
            + GROUP_SEPARATOR
            + Item::Radius.width()
            + ITEM_GAP
            + Item::Hint(Hint::RadiusApplies(2, 2)).width()
            + GROUP_SEPARATOR
            + Item::Angle.width()
            + Item::Arrange.width()
            + Item::Delete.width()
            + 2. * ITEM_GAP
            + 10.;
        let size = PinToolbarLayout::property_size(work, 1., Tool::Edit, 2, false);
        assert!((size.0 - multi).abs() < 0.01 && size.1 == 42.);
    }

    #[test]
    fn highlight_radius_pin_properties_use_shared_width_and_reserved_stack() {
        let work = work();
        for dpi in [1., 1.25, 2., 3.] {
            let layout = PinToolbarLayout::new(pin(250, 300), work, dpi);
            assert_eq!(
                PinToolbarLayout::property_size(work, dpi, Tool::HighlighterRect, 0, false),
                (479., 42.)
            );
            // DPI3 下对象段整组换到第二行，宽度收到较宽的一行。
            assert_eq!(
                PinToolbarLayout::property_size(work, dpi, Tool::HighlighterRect, 1, false),
                if dpi == 3. { (479., 78.) } else { (655., 42.) }
            );
            let secondary = properties(layout, work, dpi, Tool::HighlighterRect, 1);
            assert!(contains(work, layout.primary) && contains(work, secondary));
            assert!(!overlap(layout.primary, secondary));
        }
    }

    #[test]
    fn narrow_work_wraps_properties_and_keeps_both_windows_in_bounds() {
        let work = Placement {
            x: -600,
            y: 20,
            width: 420,
            height: 800,
        };
        let pin = Placement {
            x: -550,
            y: 350,
            width: 300,
            height: 300,
        };
        let layout = PinToolbarLayout::new(pin, work, 1.25);
        assert!(layout.properties_above);
        assert_eq!(layout.primary.width, work.width);
        for tool in TOOLS {
            let secondary = properties(layout, work, 1.25, tool, 1);
            assert!(contains(work, layout.primary) && contains(work, secondary));
            assert!(!overlap(layout.primary, secondary));
        }
    }

    #[test]
    fn very_short_work_shrinks_secondary_viewport_instead_of_overlapping_primary() {
        for (height, needs_shrink) in [(2, true), (20, true), (48, true), (55, true), (100, false)]
        {
            let work = Placement {
                x: -700,
                y: -10,
                width: 360,
                height,
            };
            let pin = Placement {
                x: -650,
                y: -10,
                width: 300,
                height,
            };
            let layout = PinToolbarLayout::new(pin, work, 1.);
            let secondary = properties(layout, work, 1., Tool::Polyline, 1);
            assert!(contains(work, layout.primary) && contains(work, secondary));
            assert!(layout.primary.height > 0 && secondary.height > 0);
            assert!(!overlap(layout.primary, secondary), "height={height}");
            let requested = pixels(
                PinToolbarLayout::property_size(work, 1., Tool::Polyline, 1, false).1,
                1.,
            );
            if needs_shrink {
                assert!(secondary.height < requested, "height={height}");
            } else {
                // 100px 能容纳 48px 一级 + 8px 间距 + 已由共享布局限制的 28px 二级。
                assert_eq!(secondary.height, requested);
            }
        }
    }

    #[test]
    fn very_short_work_with_configured_multirow_primary_keeps_two_bounded_viewports() {
        for height in [2, 20, 48, 55, 100] {
            let work = Placement {
                x: -700,
                y: -110,
                width: 360,
                height,
            };
            let pin = Placement {
                x: -650,
                width: 300,
                ..work
            };
            for dpi in [1., 1.25, 2., 3.] {
                for count in [0, 6, crate::toolbar_config::AVAILABLE_TOOLS.len()] {
                    let layout = PinToolbarLayout::new_with_tool_count(pin, work, dpi, count);
                    assert!(contains(work, layout.primary));
                    assert!(layout.primary.height > 0);
                    for tool in [Tool::Polyline, Tool::Text, Tool::Rectangle] {
                        let secondary = properties(layout, work, dpi, tool, 1);
                        assert!(contains(work, secondary));
                        assert!(secondary.height > 0);
                        assert!(!overlap(layout.primary, secondary));
                    }
                }
            }
        }
    }

    #[test]
    fn empty_and_single_pixel_work_and_invalid_dpi_remain_bounded() {
        for height in [0, 1] {
            let work = Placement {
                x: -10,
                y: -10,
                width: 1,
                height,
            };
            let layout = PinToolbarLayout::new(pin(0, 500), work, f32::NAN);
            let secondary = properties(layout, work, 0., Tool::Text, 1);
            assert!(contains(work, layout.primary) && contains(work, secondary));
        }
        assert_eq!(
            PinToolbarLayout::new(pin(100, 200), work(), -1.),
            PinToolbarLayout::new(pin(100, 200), work(), 1.)
        );
    }

    #[test]
    fn offscreen_pin_above_work_keeps_stack_below_it_at_work_top() {
        let layout = PinToolbarLayout::new(pin(-300, 240), work(), 1.);
        assert!(!layout.properties_above);
        assert_eq!(layout.primary.y, work().y);
        let secondary = properties(layout, work(), 1., Tool::Pen, 1);
        assert!(contains(work(), secondary));
        assert!(!overlap(layout.primary, secondary));
    }

    #[test]
    fn shorter_work_prefers_room_below_pin_and_scrolls_secondary_without_covering_image() {
        let work = Placement {
            x: 0,
            y: 0,
            width: 360,
            height: 300,
        };
        let pin = Placement {
            x: 25,
            y: 0,
            width: 300,
            height: 80,
        };
        let layout = PinToolbarLayout::new(pin, work, 1.);
        assert!(!layout.properties_above);
        assert_eq!(layout.primary.y, 88);
        let secondary = properties(layout, work, 1., Tool::Polyline, 1);
        // 窄屏逐级收起（色块收成颜色按钮、分段收成下拉）后，折线加对象段两行（78），不需要滚动。
        assert_eq!((secondary.y, secondary.height), (144, 78));
        assert!(contains(work, secondary));
        assert!(!overlap(pin, layout.primary) && !overlap(pin, secondary));
        assert!(!overlap(layout.primary, secondary));
        for tool in TOOLS {
            let other = properties(layout, work, 1., tool, 1);
            assert!(!overlap(pin, other) && !overlap(layout.primary, other));
        }
        // 满屏图片无法避让时，两级仍须可见，不能把属性栏夹成一像素。
        let full_pin = Placement {
            height: work.height,
            ..pin
        };
        let full_layout = PinToolbarLayout::new(full_pin, work, 1.);
        let full_secondary = properties(full_layout, work, 1., Tool::Polyline, 1);
        assert_eq!(full_secondary.height, 78);
        assert!(contains(work, full_secondary));
        assert!(!overlap(full_layout.primary, full_secondary));
    }
}
