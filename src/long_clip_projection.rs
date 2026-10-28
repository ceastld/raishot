//! 剪辑显示投影：只平移原图片段，标注与套索仍使用原图坐标。
use crate::{
    annotation::Point,
    geometry::Rect,
    long_clip::{LongClip, SeamSide},
    stitch::ScrollAxis,
};
use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionPiece {
    /// 用于底图与同一份 Scene 重放的完整跨轴原图区域。
    pub source: Rect,
    /// 显示矩形与 source 尺寸相同，不因剪辑缩放或改变内部几何。
    pub display: Rect,
    /// display.origin - source.origin；渲染同一原图坐标的场景时应用此平移。
    pub translation: (i64, i64),
    /// 被删除但临时展开的区域；应压暗显示，不进入成品。
    pub removed: bool,
    /// 真正进入输出的原图区域，跨轴已经裁剪；展开的删除区域为 None。
    pub output_source: Option<Rect>,
    /// 对应最终成品中的矩形，不包含任何临时展开区域。
    pub output: Option<Rect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionSeam {
    pub source: Range<u32>,
    pub display_at: u32,
}

/// 跨轴保留完整原图范围，便于显示并拖回两侧裁边；output_size 才是实际成品尺寸。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LongClipProjection {
    source_size: (u32, u32),
    display_size: (u32, u32),
    output_size: (u32, u32),
    axis: ScrollAxis,
    cross: Range<u32>,
    pieces: Vec<ProjectionPiece>,
    seams: Vec<ProjectionSeam>,
}

impl LongClipProjection {
    /// expanded 以当前归并后的原图区间精确标识；重复或已失效的标识不会增加片段。
    /// 不修改或重新归并 LongClip 的删除记录。
    pub(crate) fn new(clip: &LongClip, expanded: &[Range<u32>]) -> Self {
        let source_size = clip.source_size();
        let axis = clip.axis();
        let (long_len, cross_len) = along(axis, source_size);
        let mut projection = Self {
            source_size,
            display_size: source_size,
            output_size: clip.output_size(),
            axis,
            cross: clip.cross_range(),
            pieces: Vec::with_capacity(clip.removed().len() * 2 + 1),
            seams: Vec::new(),
        };
        let mut source_start = 0;
        let mut display_start = 0;
        let mut output_start = 0;
        for removed in clip.removed() {
            if source_start < removed.start {
                let length = removed.start - source_start;
                projection.push_piece(source_start, length, display_start, output_start, false);
                display_start += length;
                output_start += length;
            }
            if expanded.contains(removed) {
                let length = removed.end - removed.start;
                projection.push_piece(removed.start, length, display_start, output_start, true);
                display_start += length;
            } else {
                projection.seams.push(ProjectionSeam {
                    source: removed.clone(),
                    display_at: display_start,
                });
            }
            source_start = removed.end;
        }
        if source_start < long_len {
            let length = long_len - source_start;
            projection.push_piece(source_start, length, display_start, output_start, false);
            display_start += length;
        }
        projection.display_size = compose(axis, display_start, cross_len);
        projection
    }

    fn push_piece(
        &mut self,
        source_start: u32,
        length: u32,
        display_start: u32,
        output_start: u32,
        removed: bool,
    ) {
        let (_, cross_len) = along(self.axis, self.source_size);
        let source = axis_rect(self.axis, source_start, length, 0, cross_len);
        let display = axis_rect(self.axis, display_start, length, 0, cross_len);
        self.pieces.push(ProjectionPiece {
            source,
            display,
            translation: (
                i64::from(display.x) - i64::from(source.x),
                i64::from(display.y) - i64::from(source.y),
            ),
            removed,
            output_source: (!removed).then(|| {
                axis_rect(
                    self.axis,
                    source_start,
                    length,
                    self.cross.start,
                    self.cross.end - self.cross.start,
                )
            }),
            output: (!removed).then(|| {
                axis_rect(
                    self.axis,
                    output_start,
                    length,
                    0,
                    self.cross.end - self.cross.start,
                )
            }),
        });
    }

    pub(crate) fn source_size(&self) -> (u32, u32) {
        self.source_size
    }

    pub(crate) fn display_size(&self) -> (u32, u32) {
        self.display_size
    }

    pub(crate) fn output_size(&self) -> (u32, u32) {
        self.output_size
    }

    pub(crate) fn axis(&self) -> ScrollAxis {
        self.axis
    }

    pub(crate) fn cross_range(&self) -> Range<u32> {
        self.cross.clone()
    }

    pub(crate) fn pieces(&self) -> &[ProjectionPiece] {
        &self.pieces
    }

    pub(crate) fn seams(&self) -> &[ProjectionSeam] {
        &self.seams
    }

    /// 删除且收起的原图位置没有显示像素；展开的删除区域可以正常映射。
    /// 原图右／下几何端点允许传入，长轴末端沿用 LongClip 的特殊边界合同。
    pub(crate) fn source_to_display(&self, point: Point) -> Option<Point> {
        self.map_source_point(point, false)
    }

    /// 原图点若位于收起的删除区间，落到该区间形成的最近零宽接缝。
    /// 原图之外不进行隐式钳制；跨轴压暗区域仍按完整原图坐标映射。
    pub(crate) fn source_to_display_nearest(&self, point: Point) -> Option<Point> {
        self.map_source_point(point, true)
    }

    fn map_source_point(&self, point: Point, nearest: bool) -> Option<Point> {
        if !within(point, self.source_size) {
            return None;
        }
        let (long, cross) = along(self.axis, (point.x, point.y));
        let (source_long, _) = along(self.axis, self.source_size);
        let (display_long, _) = along(self.axis, self.display_size);
        let projected = if long == source_long as f32 {
            display_long as f32
        } else {
            let mut deleted = 0;
            let mut collapsed_at = None;
            for seam in &self.seams {
                if long < seam.source.start as f32 {
                    break;
                }
                if long < seam.source.end as f32 {
                    if !nearest {
                        return None;
                    }
                    collapsed_at = Some(seam.display_at as f32);
                    break;
                }
                deleted += seam.source.end - seam.source.start;
            }
            collapsed_at.unwrap_or(long - deleted as f32)
        };
        let (x, y) = compose(self.axis, projected, cross);
        Some(Point { x, y })
    }

    /// 收起接缝的 Before／After 分别返回被删原图区间起点／终点。
    /// 已展开区域没有零宽接缝，按其原图像素位置直接换算。
    pub(crate) fn display_to_source(&self, point: Point, side: SeamSide) -> Option<Point> {
        if !within(point, self.display_size) {
            return None;
        }
        let (long, cross) = along(self.axis, (point.x, point.y));
        let mut deleted = 0;
        for seam in &self.seams {
            if long < seam.display_at as f32 {
                break;
            }
            if long == seam.display_at as f32 {
                let source = match side {
                    SeamSide::Before => seam.source.start as f32,
                    SeamSide::After => seam.source.end as f32,
                };
                let (x, y) = compose(self.axis, source, cross);
                return Some(Point { x, y });
            }
            deleted += seam.source.end - seam.source.start;
        }
        let (x, y) = compose(self.axis, long + deleted as f32, cross);
        Some(Point { x, y })
    }

    /// 返回同一原图锚点在 after 与当前投影中的显示位置差（after - before）。
    ///
    /// 屏幕位置 = display * zoom + Viewport.offset，因此 offset 应减去 delta * zoom。
    /// 若调用者使用正向 scroll 值，则 scroll 应加上 delta * zoom。删除内的锚点落到接缝。
    pub(crate) fn anchor_delta(&self, after: &Self, source_anchor: Point) -> Option<Point> {
        if self.source_size != after.source_size || self.axis != after.axis {
            return None;
        }
        let before = self.source_to_display_nearest(source_anchor)?;
        let after = after.source_to_display_nearest(source_anchor)?;
        Some(Point {
            x: after.x - before.x,
            y: after.y - before.y,
        })
    }
}

fn along<T>(axis: ScrollAxis, size: (T, T)) -> (T, T) {
    match axis {
        ScrollAxis::Vertical => (size.1, size.0),
        ScrollAxis::Horizontal => size,
    }
}

fn compose<T>(axis: ScrollAxis, long: T, cross: T) -> (T, T) {
    match axis {
        ScrollAxis::Vertical => (cross, long),
        ScrollAxis::Horizontal => (long, cross),
    }
}

fn axis_rect(axis: ScrollAxis, start: u32, length: u32, cross: u32, breadth: u32) -> Rect {
    let (x, y) = compose(axis, start, cross);
    let (width, height) = compose(axis, length, breadth);
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn within(point: Point, size: (u32, u32)) -> bool {
    point.x.is_finite()
        && point.y.is_finite()
        && point.x >= 0.
        && point.y >= 0.
        && point.x <= size.0 as f32
        && point.y <= size.1 as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip() -> LongClip {
        let mut clip = LongClip::new(100, 100, ScrollAxis::Vertical).unwrap();
        clip.delete(0..10).unwrap();
        clip.delete(30..40).unwrap();
        clip.delete(80..100).unwrap();
        clip.set_cross(20..90).unwrap();
        clip
    }

    #[test]
    fn long_clip_projection_keeps_cross_axis_chrome_outside_output() {
        let source = clip();
        let projection = LongClipProjection::new(&source, &[]);
        assert_eq!(projection.display_size(), (100, 60));
        assert_eq!(projection.output_size(), (70, 60));
        assert_eq!(projection.source_size(), (100, 100));
        assert_eq!(projection.axis(), ScrollAxis::Vertical);
        assert_eq!(projection.cross_range(), 20..90);
        assert_eq!(projection.pieces().len(), 2);
        let first = &projection.pieces()[0];
        assert_eq!(
            first.source,
            Rect {
                x: 0,
                y: 10,
                width: 100,
                height: 20
            }
        );
        assert_eq!(
            first.display,
            Rect {
                x: 0,
                y: 0,
                width: 100,
                height: 20
            }
        );
        assert_eq!(first.translation, (0, -10));
        assert_eq!(
            first.output_source,
            Some(Rect {
                x: 20,
                y: 10,
                width: 70,
                height: 20
            })
        );
        assert_eq!(
            first.output,
            Some(Rect {
                x: 0,
                y: 0,
                width: 70,
                height: 20
            })
        );
        let last = &projection.pieces()[1];
        assert_eq!(last.translation, (0, -20));
        assert_eq!(
            last.output,
            Some(Rect {
                x: 0,
                y: 20,
                width: 70,
                height: 40
            })
        );
        assert!(projection.pieces().iter().all(|piece| !piece.removed));
        assert_eq!(
            projection.source_to_display(Point { x: 5., y: 50. }),
            Some(Point { x: 5., y: 30. })
        );
    }

    #[test]
    fn long_clip_projection_expands_head_and_tail_without_changing_output() {
        let source = clip();
        let unchanged = source.clone();
        let projection = LongClipProjection::new(&source, &[0..10, 80..100]);
        assert_eq!(projection.display_size(), (100, 90));
        assert_eq!(projection.output_size(), (70, 60));
        assert_eq!(projection.pieces().len(), 4);
        assert!(projection.pieces()[0].removed);
        assert!(projection.pieces()[3].removed);
        assert_eq!(
            projection.pieces()[3].display,
            Rect {
                x: 0,
                y: 70,
                width: 100,
                height: 20
            }
        );
        assert_eq!(projection.pieces()[3].output, None);
        assert_eq!(projection.pieces()[3].output_source, None);
        assert_eq!(
            projection.seams(),
            &[ProjectionSeam {
                source: 30..40,
                display_at: 30
            }]
        );
        assert_eq!(
            projection.source_to_display(Point { x: 50., y: 85. }),
            Some(Point { x: 50., y: 75. })
        );
        for side in [SeamSide::Before, SeamSide::After] {
            assert_eq!(
                projection.display_to_source(Point { x: 50., y: 75. }, side),
                Some(Point { x: 50., y: 85. })
            );
        }
        assert_eq!(source, unchanged);
    }

    #[test]
    fn long_clip_projection_all_expanded_is_an_identity_display() {
        let source = clip();
        let projection = LongClipProjection::new(&source, source.removed());
        assert_eq!(projection.display_size(), source.source_size());
        assert_eq!(projection.output_size(), source.output_size());
        assert!(projection.seams().is_empty());
        assert!(projection
            .pieces()
            .iter()
            .all(|piece| piece.translation == (0, 0)));
        for point in [
            Point { x: 0., y: 0. },
            Point { x: 50., y: 35. },
            Point { x: 100., y: 100. },
        ] {
            assert_eq!(projection.source_to_display(point), Some(point));
            assert_eq!(
                projection.display_to_source(point, SeamSide::Before),
                Some(point)
            );
        }
        let stale = LongClipProjection::new(&source, &[31..39, 30..40, 30..40]);
        assert_eq!(stale, LongClipProjection::new(&source, &[30..40]));
    }

    #[test]
    fn long_clip_projection_horizontal_pieces_preserve_original_y() {
        let mut clip = LongClip::new(120, 80, ScrollAxis::Horizontal).unwrap();
        clip.delete(20..40).unwrap();
        clip.set_cross(5..75).unwrap();
        let projection = LongClipProjection::new(&clip, &[]);
        assert_eq!(projection.display_size(), (100, 80));
        assert_eq!(projection.output_size(), (100, 70));
        let piece = &projection.pieces()[1];
        assert_eq!(
            piece.source,
            Rect {
                x: 40,
                y: 0,
                width: 80,
                height: 80
            }
        );
        assert_eq!(
            piece.display,
            Rect {
                x: 20,
                y: 0,
                width: 80,
                height: 80
            }
        );
        assert_eq!(piece.translation, (-20, 0));
        assert_eq!(
            piece.output_source,
            Some(Rect {
                x: 40,
                y: 5,
                width: 80,
                height: 70
            })
        );
        assert_eq!(
            piece.output,
            Some(Rect {
                x: 20,
                y: 0,
                width: 80,
                height: 70
            })
        );
        assert_eq!(projection.source_to_display(Point { x: 35., y: 2. }), None);
        assert_eq!(
            projection.source_to_display_nearest(Point { x: 35., y: 2. }),
            Some(Point { x: 20., y: 2. })
        );
        assert_eq!(
            projection.display_to_source(Point { x: 20., y: 70. }, SeamSide::Before),
            Some(Point { x: 20., y: 70. })
        );
        assert_eq!(
            projection.display_to_source(Point { x: 20., y: 70. }, SeamSide::After),
            Some(Point { x: 40., y: 70. })
        );
        let expanded = LongClipProjection::new(&clip, &[20..40]);
        assert_eq!(expanded.display_size(), (120, 80));
        assert_eq!(
            projection.anchor_delta(&expanded, Point { x: 60., y: 2. }),
            Some(Point { x: 20., y: 0. })
        );
    }

    #[test]
    fn long_clip_projection_seams_follow_the_model_boundary_contract() {
        let source = clip();
        let projection = LongClipProjection::new(&source, &[]);
        for (display, before, after) in [(0., 0., 10.), (20., 30., 40.), (60., 80., 100.)] {
            for (side, expected) in [(SeamSide::Before, before), (SeamSide::After, after)] {
                assert_eq!(
                    projection.display_to_source(Point { x: 50., y: display }, side),
                    Some(Point {
                        x: 50.,
                        y: expected
                    })
                );
                assert_eq!(source.display_to_source(display, side), Some(expected));
            }
        }
        for position in [0., 10., 29.5, 30., 35., 40., 79.5, 80., 100.] {
            assert_eq!(
                projection
                    .source_to_display(Point {
                        x: 50.,
                        y: position
                    })
                    .map(|p| p.y),
                source.source_to_display(position)
            );
        }
        assert_eq!(
            projection.source_to_display_nearest(Point { x: 50., y: 35. }),
            Some(Point { x: 50., y: 20. })
        );
        assert_eq!(
            projection.source_to_display(Point { x: 101., y: 50. }),
            None
        );
        assert_eq!(
            projection.display_to_source(Point { x: 50., y: 61. }, SeamSide::After),
            None
        );
        assert_eq!(
            projection.display_to_source(Point { x: f32::NAN, y: 0. }, SeamSide::After),
            None
        );
    }

    #[test]
    fn long_clip_projection_toggle_anchor_keeps_the_same_screen_position() {
        let source = clip();
        let before = LongClipProjection::new(&source, &[]);
        let after = LongClipProjection::new(&source, source.removed());
        let zoom = 0.5;
        let old_offset = Point { x: 12., y: -30. };
        for anchor in [Point { x: 50., y: 70. }, Point { x: 50., y: 35. }] {
            let delta = before.anchor_delta(&after, anchor).unwrap();
            let old = before.source_to_display_nearest(anchor).unwrap();
            let new = after.source_to_display_nearest(anchor).unwrap();
            let next_offset = Point {
                x: old_offset.x - delta.x * zoom,
                y: old_offset.y - delta.y * zoom,
            };
            assert_eq!(old.x * zoom + old_offset.x, new.x * zoom + next_offset.x);
            assert_eq!(old.y * zoom + old_offset.y, new.y * zoom + next_offset.y);
            assert_eq!(
                after.anchor_delta(&before, anchor),
                Some(Point {
                    x: -delta.x,
                    y: -delta.y
                })
            );
        }
        let unrelated =
            LongClipProjection::new(&LongClip::new(120, 100, ScrollAxis::Vertical).unwrap(), &[]);
        assert_eq!(
            before.anchor_delta(&unrelated, Point { x: 50., y: 70. }),
            None
        );
    }
}
