use super::*;
use crate::viewport::Viewport;

#[test]
fn long_clip_input_cross_drag_past_opposite_edge_keeps_the_fixed_side() {
    for edge in [ClipEdge::CrossStart, ClipEdge::CrossEnd] {
        for value in 0..=320 {
            let range = cross_drag_range(100..300, edge, value, 320);
            assert!(range.start < range.end && range.end <= 320);
            if edge == ClipEdge::CrossStart {
                assert_eq!(range.end, 300, "拖起侧不能移动固定的末侧");
            } else {
                assert_eq!(range.start, 100, "拖末侧不能移动固定的起侧");
            }
        }
    }
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let (width, height) = axis.dimensions(320, 1_000);
        let mut clip = LongClip::new(width, height, axis).unwrap();
        clip.trim(100..900, 100..300).unwrap();
        let range = cross_drag_range(clip.cross_range(), ClipEdge::CrossEnd, 50, 320);
        clip.trim(100..900, range).unwrap();
        assert_eq!(clip.cross_range(), 100..101);
        assert_eq!(clip.output_size(), axis.dimensions(1, 800));
        assert_eq!(clip.removed(), &[0..100, 900..1_000]);
    }
}

#[test]
fn long_clip_input_outer_trim_expands_without_jumping_and_recovers_only_dragged_pixels() {
    for axis in [ScrollAxis::Vertical, ScrollAxis::Horizontal] {
        let (width, height) = axis.dimensions(320, 1_000);
        let mut clip = LongClip::new(width, height, axis).unwrap();
        clip.trim(100..900, 0..320).unwrap();
        let before = LongClipProjection::new(&clip, &[]);
        for edge in [ClipEdge::LongStart, ClipEdge::LongEnd] {
            for zoom in [0.375, 1., 1.5] {
                let mut viewport = Viewport {
                    zoom,
                    offset: Point { x: 80., y: 120. },
                };
                let at = if edge == ClipEdge::LongStart {
                    100
                } else {
                    900
                };
                let boundary = compose(axis, at as f32, 160.);
                let screen =
                    viewport.window_point(before.source_to_display_nearest(boundary).unwrap());
                // 普通输入在末接缝的 After 是原图末端，不是实际保留边界。
                let pointer_source = before
                    .display_to_source(viewport.image_point(screen), SeamSide::After)
                    .unwrap();
                assert_eq!(
                    along(axis, pointer_source).0,
                    if edge == ClipEdge::LongStart {
                        100.
                    } else {
                        1_000.
                    }
                );
                let drag = ClipDrag::Trim {
                    start: screen,
                    moved: false,
                    edge,
                    before: clip.clone(),
                    long_keep: 100..900,
                    cross_keep: 0..320,
                    expanded: Vec::new(),
                };
                let expanded = if edge == ClipEdge::LongStart {
                    vec![0..100]
                } else {
                    vec![900..1_000]
                };
                let after = LongClipProjection::new(&clip, &expanded);
                let delta = before
                    .anchor_delta(&after, expansion_anchor(&drag, axis, pointer_source))
                    .unwrap();
                viewport.offset.x -= delta.x * zoom;
                viewport.offset.y -= delta.y * zoom;
                let opened =
                    viewport.window_point(after.source_to_display_nearest(boundary).unwrap());
                assert!(opened.distance(screen) < 0.001, "展开裁边移动了原保留边界");

                let (long, cross) = along(axis, screen);
                let direction = if edge == ClipEdge::LongStart { -1. } else { 1. };
                let moved = compose(axis, long + direction * 10. * zoom, cross);
                let source = after
                    .display_to_source(viewport.image_point(moved), SeamSide::After)
                    .unwrap();
                let value = along(axis, source).0.round() as u32;
                assert_eq!(value, if edge == ClipEdge::LongStart { 90 } else { 910 });
                let mut candidate = clip.clone();
                candidate
                    .trim(
                        if edge == ClipEdge::LongStart {
                            value..900
                        } else {
                            100..value
                        },
                        0..320,
                    )
                    .unwrap();
                assert_eq!(axis, candidate.axis());
                assert_eq!(lengths(&candidate).0, 1_000);
                assert_eq!(candidate.output_size(), axis.dimensions(320, 810));
                assert_eq!(clip.removed(), &[0..100, 900..1_000]);
            }
        }
    }
}
