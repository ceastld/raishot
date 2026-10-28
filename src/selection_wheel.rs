use crate::{editor::Editor, geometry::Rect, interaction::Parameter};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfirmedWheelRoute {
    Parameter(Parameter),
    Center,
}

/// 截图中心回退不借滚轮提交任何草稿；已有参数路线由适配层单独保留。
pub(crate) fn center_ready(editor: &Editor, busy: bool, input_active: bool) -> bool {
    !busy
        && !input_active
        && !editor.is_dragging()
        && !editor.has_pending_drawing()
        && editor.text_draft.is_none()
        && editor.lasso_draft.is_none()
        && !editor.watermark.has_preview()
}

/// 仅普通截图的确认范围调用；映射就表示参数消费，不能用 changed 决定回退。
pub(crate) fn confirmed_route(
    editor: &Editor,
    point: (i32, i32),
    shift: bool,
    control: bool,
    pointer_active: bool,
    input_active: bool,
) -> Option<ConfirmedWheelRoute> {
    // overlay 调整门仅对应选区移动/缩放；内部绘图参数仍保留原手势合同。
    if editor.is_changing_crop() || !editor.canvas.contains(point) {
        return None;
    }
    let crop = editor
        .crop
        .filter(|crop| crop.valid(editor.canvas.width, editor.canvas.height))?;
    if crop.contains(point) && editor.tool != crate::annotation::Tool::Crop {
        if let Some(parameter) = editor
            .wheel_parameter(shift, control)
            .filter(|parameter| *parameter != Parameter::Crop)
        {
            return Some(ConfirmedWheelRoute::Parameter(parameter));
        }
    }
    center_ready(editor, pointer_active, input_active).then_some(ConfirmedWheelRoute::Center)
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        crop_mask::Polygon,
        interaction::WheelAccumulator,
    };
    use std::time::{Duration, Instant};

    fn editor(tool: Tool) -> Editor {
        let mut editor = Editor::new(
            Rect::full(400, 300),
            Some(Rect {
                x: 100,
                y: 80,
                width: 120,
                height: 100,
            }),
        );
        editor.select_tool(tool);
        editor
    }
    fn route(editor: &Editor, point: (i32, i32)) -> Option<ConfirmedWheelRoute> {
        confirmed_route(editor, point, false, false, false, false)
    }

    #[test]
    fn confirmed_empty_edit_crop_and_outside_drawing_tools_use_center_with_all_modifiers() {
        for tool in [Tool::Edit, Tool::Crop, Tool::Rectangle] {
            let editor = editor(tool);
            for point in [(50, 130), (97, 130), (250, 130)] {
                for shift in [false, true] {
                    for control in [false, true] {
                        assert_eq!(
                            confirmed_route(&editor, point, shift, control, false, false),
                            Some(ConfirmedWheelRoute::Center)
                        );
                    }
                }
            }
        }
        for tool in [Tool::Edit, Tool::Crop] {
            assert_eq!(
                route(&editor(tool), (160, 130)),
                Some(ConfirmedWheelRoute::Center)
            );
        }
        assert_eq!(
            route(&editor(Tool::Rectangle), (160, 130)),
            Some(ConfirmedWheelRoute::Parameter(Parameter::Width))
        );
    }

    #[test]
    fn mapped_parameter_consumes_half_notches_and_limits_without_center_fallback() {
        let mut editor = editor(Tool::Edit);
        let mut annotation = Annotation::new(
            Tool::Rectangle,
            Point { x: 130., y: 100. },
            Style {
                width: 50.,
                ..Style::default()
            },
        );
        annotation.update(Point { x: 180., y: 150. });
        let id = editor.document.insert(annotation);
        editor.selected = Some(id);
        editor.style.width = 50.;
        let crop = editor.crop;
        let mut wheel = WheelAccumulator::default();
        let now = Instant::now();
        for (distance, at) in [(0.5, now), (0.5, now + Duration::from_millis(20))] {
            let Some(ConfirmedWheelRoute::Parameter(parameter)) = route(&editor, (160, 130)) else {
                panic!("mapped parameter must consume");
            };
            let (steps, group) = wheel.push(parameter, distance, at);
            assert!(!editor.adjust_parameter(parameter, steps, group, (160, 130), 6.));
            assert_eq!(editor.crop, crop);
        }
        let style = editor.style;
        assert_eq!(route(&editor, (50, 130)), Some(ConfirmedWheelRoute::Center));
        assert!(editor.adjust_crop_from_center(1));
        assert_eq!(editor.style, style);
        assert_eq!(editor.selected, Some(id));
        editor.tool = Tool::Crop;
        assert_eq!(
            route(&editor, (160, 130)),
            Some(ConfirmedWheelRoute::Center)
        );
    }

    #[test]
    fn center_rejects_pending_text_but_preserves_inside_font_during_pointer_and_input() {
        let mut editor = editor(Tool::Text);
        editor.begin_text(None, Point { x: 140., y: 120. });
        editor.set_text("未提交文字".into());
        let revision = editor.document.revision();
        assert_eq!(route(&editor, (50, 130)), None);
        assert_eq!(
            confirmed_route(&editor, (160, 130), false, false, false, true),
            Some(ConfirmedWheelRoute::Parameter(Parameter::Font))
        );
        assert_eq!(
            confirmed_route(&editor, (160, 130), false, false, true, false),
            Some(ConfirmedWheelRoute::Parameter(Parameter::Font))
        );
        assert!(!editor.adjust_crop_from_center(1));
        assert!(editor.text_draft.is_some());
        assert_eq!(editor.document.revision(), revision);
        for point in [(-1, 130), (400, 130)] {
            assert_eq!(route(&editor, point), None);
        }
        editor.set_crop(None);
        assert_eq!(route(&editor, (160, 130)), None);
    }

    #[test]
    fn inside_draw_and_object_parameters_preserve_gestures_but_crop_adjustment_rejects() {
        let mut drawing = editor(Tool::Pen);
        drawing.pointer_down((140, 120), 6.);
        drawing.pointer_move((160, 130));
        assert!(drawing.is_dragging());
        assert!(!drawing.is_changing_crop());
        assert_eq!(
            confirmed_route(&drawing, (160, 130), false, false, true, false),
            Some(ConfirmedWheelRoute::Parameter(Parameter::Width))
        );
        assert_eq!(
            confirmed_route(&drawing, (50, 130), false, false, true, false),
            None
        );
        assert!(drawing.is_dragging());
        assert!(drawing.document.items().is_empty());

        let mut moving = editor(Tool::Edit);
        let mut annotation = Annotation::new(
            Tool::Rectangle,
            Point { x: 130., y: 100. },
            Style::default(),
        );
        annotation.update(Point { x: 180., y: 150. });
        let id = moving.document.insert(annotation);
        moving.selected = Some(id);
        moving.selection = vec![id];
        moving.pointer_down((160, 130), 6.);
        assert!(moving.is_dragging());
        assert!(!moving.is_changing_crop());
        assert_eq!(
            confirmed_route(&moving, (160, 130), false, false, true, false),
            Some(ConfirmedWheelRoute::Parameter(Parameter::Width))
        );
        assert_eq!(
            confirmed_route(&moving, (50, 130), false, false, true, false),
            None
        );
        assert!(moving.is_dragging());
        assert_eq!(moving.selected, Some(id));

        let mut resizing = editor(Tool::Crop);
        resizing.pointer_down((100, 130), 6.);
        assert!(resizing.is_changing_crop());
        assert_eq!(
            confirmed_route(&resizing, (160, 130), false, false, true, false),
            None
        );
        assert!(resizing.is_changing_crop());
    }

    #[test]
    fn center_editor_commit_keeps_fullscreen_objects_selection_style_and_object_undo() {
        let mut editor = editor(Tool::Edit);
        let mut annotation =
            Annotation::new(Tool::Rectangle, Point { x: 50., y: 40. }, Style::default());
        annotation.update(Point { x: 90., y: 70. });
        let id = editor.document.insert(annotation);
        editor.selected = Some(id);
        editor.selection = vec![id];
        editor.document.set_crop_mask(Polygon::new(&[
            Point { x: 100., y: 80. },
            Point { x: 220., y: 80. },
            Point { x: 220., y: 180. },
            Point { x: 100., y: 180. },
        ]));
        let annotations = editor.document_annotations();
        let stamp = editor.scene_stamp();
        let style = editor.style;
        assert!(editor.adjust_crop_from_center(1));
        assert_eq!(
            editor.crop,
            Some(Rect {
                x: 99,
                y: 79,
                width: 122,
                height: 102
            })
        );
        assert!(editor.document.crop_mask.is_none());
        assert_ne!(editor.scene_stamp(), stamp);
        assert_eq!(editor.document_annotations(), annotations);
        assert_eq!(editor.selected, Some(id));
        assert_eq!(editor.selected_ids(), vec![id]);
        assert_eq!(editor.style, style);
        assert_eq!(editor.tool, Tool::Edit);
        editor.undo();
        assert!(editor.document.items().is_empty());
        editor.redo();
        assert_eq!(editor.document_annotations(), annotations);
    }

    #[test]
    fn center_no_change_preserves_mask_scene_and_history_at_full_or_minimum_size() {
        for (crop, steps) in [
            (Rect::full(400, 300), 1),
            (
                Rect {
                    x: 100,
                    y: 80,
                    width: 2,
                    height: 2,
                },
                -1,
            ),
        ] {
            let mut editor = editor(Tool::Edit);
            editor.crop = Some(crop);
            editor.document.set_crop_mask(Polygon::new(&[
                Point { x: 100., y: 80. },
                Point { x: 102., y: 80. },
                Point { x: 102., y: 82. },
                Point { x: 100., y: 82. },
            ]));
            let mask = editor.document.crop_mask.clone();
            let stamp = editor.scene_stamp();
            let revision = editor.document.edit_revision();
            assert!(!editor.adjust_crop_from_center(steps));
            assert_eq!(editor.crop, Some(crop));
            assert_eq!(editor.document.crop_mask, mask);
            assert_eq!(editor.scene_stamp(), stamp);
            assert_eq!(editor.document.edit_revision(), revision);
        }
    }
}

/// 从中心按完整刻度扩缩确认选区；只接受原点为零的完整物理像素画布。
///
/// 每个正刻度四边理想各扩 1 像素，宽高分别饱和后平移进画布；每个负刻度
/// 只在两轴都能减 2 且仍至少 1 物理像素时同时收缩。最小值与逐轴尺寸饱和
/// 是有意的安全合同：不采用 1 DIP 最小值，也不允许越界的大尺寸。
/// 此函数只返回几何结果，不提交 Editor、mask、Scene 或历史状态。
pub(crate) fn crop_from_center(canvas: Rect, crop: Rect, steps: i32) -> Option<Rect> {
    if canvas.x != 0
        || canvas.y != 0
        || !canvas.valid(canvas.width, canvas.height)
        || !crop.valid(canvas.width, canvas.height)
    {
        return None;
    }
    if steps == 0 {
        return Some(crop);
    }

    // 验证已使用 Rect::valid 的 checked_add；之后先提升到 i64 再做所有运算。
    // u32 尺寸加两倍 i32 刻度最多为 8_589_934_589，仍远低于 i64::MAX。
    let (x, y, width, height) = (
        i64::from(crop.x),
        i64::from(crop.y),
        i64::from(crop.width),
        i64::from(crop.height),
    );
    let steps = i64::from(steps);
    let (x, y, width, height) = if steps > 0 {
        let canvas_width = i64::from(canvas.width);
        let canvas_height = i64::from(canvas.height);
        let width = (width + 2 * steps).min(canvas_width);
        let height = (height + 2 * steps).min(canvas_height);

        // 第 n 步尺寸是 min(原尺寸 + 2n, 画布尺寸)。位置上界每步下降 2，
        // 最后饱和步可下降 1，随后固定为 0。右边一旦触发平移，饱和前后续
        // 上界至少下降 1，旧平移不会使位置脱离下一步上界；饱和后的 clamp
        // 区间退化为 [0, 0]。左边触边则保持 0。
        // 因而 clamp(原位置 - n, 0, 最终上界) 与逐步 clamp 完全等价，
        // 包含贴边累计平移、单轴提前饱和及饱和后继续多个刻度。
        (
            (x - steps).clamp(0, canvas_width - width),
            (y - steps).clamp(0, canvas_height - height),
            width,
            height,
        )
    } else {
        // 先提升再取负，避免 i32::MIN.abs() 溢出。任一轴到最小值后，
        // 余下刻度都无变化；两轴允许次数的最小值就是实际同时收缩次数。
        let shrink = (-steps).min((width - 1) / 2).min((height - 1) / 2);
        (
            x + shrink,
            y + shrink,
            width - 2 * shrink,
            height - 2 * shrink,
        )
    };

    Some(Rect {
        x: u32::try_from(x).ok()?,
        y: u32::try_from(y).ok()?,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn centered_notches_resize_both_axes_without_drifting() {
        let canvas = Rect::full(100, 80);
        let crop = rect(30, 20, 11, 9);
        for (steps, expected) in [
            (0, crop),
            (1, rect(29, 19, 13, 11)),
            (3, rect(27, 17, 17, 15)),
            (-1, rect(31, 21, 9, 7)),
            (-3, rect(33, 23, 5, 3)),
        ] {
            assert_eq!(crop_from_center(canvas, crop, steps), Some(expected));
            assert_eq!(2 * expected.x + expected.width, 2 * crop.x + crop.width);
            assert_eq!(2 * expected.y + expected.height, 2 * crop.y + crop.height);
        }
    }

    #[test]
    fn positive_notches_translate_at_all_four_corners() {
        let canvas = Rect::full(100, 80);
        for (crop, once, three) in [
            (rect(0, 0, 7, 5), rect(0, 0, 9, 7), rect(0, 0, 13, 11)),
            (rect(93, 0, 7, 5), rect(91, 0, 9, 7), rect(87, 0, 13, 11)),
            (rect(0, 75, 7, 5), rect(0, 73, 9, 7), rect(0, 69, 13, 11)),
            (rect(93, 75, 7, 5), rect(91, 73, 9, 7), rect(87, 69, 13, 11)),
        ] {
            assert_eq!(crop_from_center(canvas, crop, 1), Some(once));
            assert_eq!(crop_from_center(canvas, crop, 3), Some(three));
        }
    }

    #[test]
    fn saturating_one_axis_keeps_the_other_growing() {
        let canvas = Rect::full(20, 30);
        assert_eq!(
            crop_from_center(canvas, rect(0, 10, 20, 3), 1),
            Some(rect(0, 9, 20, 5))
        );
        assert_eq!(
            crop_from_center(canvas, rect(6, 0, 3, 30), 1),
            Some(rect(5, 0, 5, 30))
        );
        // 宽只剩 1 像素余量时仍保留饱和后的新尺寸，并让高度继续增长。
        assert_eq!(
            crop_from_center(canvas, rect(1, 3, 19, 7), 1),
            Some(rect(0, 2, 20, 9))
        );
        assert_eq!(
            crop_from_center(canvas, rect(0, 10, 20, 3), 20),
            Some(canvas)
        );
        assert_eq!(crop_from_center(canvas, canvas, 20), Some(canvas));
    }

    #[test]
    fn narrow_axis_stops_both_and_keeps_odd_even_minima() {
        let canvas = Rect::full(100, 80);
        for (narrow, shift, final_narrow) in [(1, 0, 1), (2, 0, 2), (3, 1, 1), (4, 1, 2)] {
            assert_eq!(
                crop_from_center(canvas, rect(10, 10, narrow, 13), -20),
                Some(rect(10 + shift, 10 + shift, final_narrow, 13 - 2 * shift))
            );
            assert_eq!(
                crop_from_center(canvas, rect(10, 10, 13, narrow), -20),
                Some(rect(10 + shift, 10 + shift, 13 - 2 * shift, final_narrow))
            );
        }
        assert_eq!(
            crop_from_center(canvas, rect(10, 10, 5, 7), -20),
            Some(rect(12, 12, 1, 3))
        );
        assert_eq!(
            crop_from_center(canvas, rect(10, 10, 8, 10), -20),
            Some(rect(13, 13, 2, 4))
        );
    }

    #[test]
    fn edge_growth_then_shrink_does_not_restore_old_position() {
        let canvas = Rect::full(100, 80);
        for (original, grown, shrunk) in [
            (rect(0, 0, 10, 8), rect(0, 0, 12, 10), rect(1, 1, 10, 8)),
            (
                rect(90, 72, 10, 8),
                rect(88, 70, 12, 10),
                rect(89, 71, 10, 8),
            ),
        ] {
            assert_eq!(crop_from_center(canvas, original, 1), Some(grown));
            assert_eq!(crop_from_center(canvas, grown, -1), Some(shrunk));
            assert_ne!(shrunk, original);
        }
    }

    #[test]
    fn invalid_ranges_are_rejected_even_at_zero_steps() {
        let crop = rect(1, 1, 3, 3);
        for canvas in [
            Rect::full(0, 10),
            Rect::full(10, 0),
            rect(1, 0, 10, 10),
            rect(0, 1, 10, 10),
            rect(u32::MAX, 0, 2, 10),
        ] {
            for steps in [0, 1, i32::MIN] {
                assert_eq!(crop_from_center(canvas, crop, steps), None);
            }
        }
        let canvas = Rect::full(u32::MAX, u32::MAX);
        for invalid in [
            rect(0, 0, 0, 1),
            rect(0, 0, 1, 0),
            rect(u32::MAX, 0, 1, 1),
            rect(0, u32::MAX, 1, 1),
            rect(u32::MAX - 1, 0, 3, 1),
            rect(0, u32::MAX - 1, 1, 3),
        ] {
            for steps in [0, 1, i32::MIN] {
                assert_eq!(crop_from_center(canvas, invalid, steps), None);
            }
        }
        assert_eq!(
            crop_from_center(Rect::full(10, 10), rect(9, 9, 2, 1), 0),
            None
        );
        assert_eq!(
            crop_from_center(Rect::full(10, 10), rect(9, 9, 1, 2), 0),
            None
        );
    }

    #[test]
    fn batched_notches_match_repeated_single_notches() {
        // 遍历小画布所有合法矩形，比较真实 helper 的批量与逐单刻度调用。
        // 固定样例已独立约束单刻度行为；这里专门检查闭式计算的时序等价。
        for (canvas_width, canvas_height) in [(5, 4), (7, 6), (10, 9)] {
            let canvas = Rect::full(canvas_width, canvas_height);
            for x in 0..canvas_width {
                for y in 0..canvas_height {
                    for width in 1..=canvas_width - x {
                        for height in 1..=canvas_height - y {
                            let crop = rect(x, y, width, height);
                            for steps in [-12_i32, -7, -3, -1, 0, 1, 2, 3, 7, 12] {
                                let mut repeated = crop;
                                for _ in 0..steps.unsigned_abs() {
                                    repeated = crop_from_center(canvas, repeated, steps.signum())
                                        .expect("合法矩形逐刻度后仍合法");
                                }
                                let batched = crop_from_center(canvas, crop, steps)
                                    .expect("合法矩形批量刻度后仍合法");
                                assert_eq!(batched, repeated, "crop={crop:?}, steps={steps}");
                                assert!(batched.valid(canvas_width, canvas_height));
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn extreme_steps_and_u32_sizes_use_bounded_wide_arithmetic() {
        let canvas = Rect::full(u32::MAX, u32::MAX);
        let crop = rect(7, 9, 3, 5);
        assert_eq!(crop_from_center(canvas, crop, i32::MAX), Some(canvas));
        assert_eq!(
            crop_from_center(canvas, crop, i32::MIN),
            Some(rect(8, 10, 1, 3))
        );
        assert_eq!(
            crop_from_center(canvas, canvas, i32::MIN),
            Some(rect(i32::MAX as u32, i32::MAX as u32, 1, 1))
        );
        let near_limit = rect(u32::MAX - 3, u32::MAX - 3, 3, 3);
        assert_eq!(crop_from_center(canvas, near_limit, 0), Some(near_limit));
        assert_eq!(
            crop_from_center(canvas, near_limit, 1),
            Some(rect(u32::MAX - 5, u32::MAX - 5, 5, 5))
        );
        assert_eq!(
            crop_from_center(canvas, near_limit, i32::MIN),
            Some(rect(u32::MAX - 2, u32::MAX - 2, 1, 1))
        );
        let tiny_canvas = Rect::full(1, 2);
        assert_eq!(
            crop_from_center(tiny_canvas, tiny_canvas, i32::MAX),
            Some(tiny_canvas)
        );
        assert_eq!(
            crop_from_center(tiny_canvas, tiny_canvas, i32::MIN),
            Some(tiny_canvas)
        );
    }
}
