//! 长图剪辑的临时交互状态。候选几何与展开状态不进入文档历史。
use crate::{
    annotation::Point,
    long_clip::{BlankSnaps, LongClip},
};
use gpui::MouseButton;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipEdge {
    LongStart,
    LongEnd,
    CrossStart,
    CrossEnd,
}

#[derive(Clone)]
pub(crate) enum ClipDrag {
    Select {
        start: Point,
        anchor: u32,
        moved: bool,
        before_selection: Option<Range<u32>>,
        before_anchor: Option<u32>,
        before_end: bool,
    },
    SelectionEdge {
        start: Point,
        moved: bool,
        fixed: u32,
        before_selection: Option<Range<u32>>,
        before_anchor: Option<u32>,
        before_end: bool,
    },
    Adjust {
        start: Point,
        moved: bool,
        index: usize,
        start_edge: bool,
        before: LongClip,
        range: Range<u32>,
        expanded: Vec<Range<u32>>,
    },
    Trim {
        start: Point,
        moved: bool,
        edge: ClipEdge,
        before: LongClip,
        long_keep: Range<u32>,
        cross_keep: Range<u32>,
        expanded: Vec<Range<u32>>,
    },
    Pan {
        button: MouseButton,
        start: Point,
        offset: Point,
    },
}

pub(crate) struct LongClipUi {
    pub(crate) expanded: Vec<Range<u32>>,
    pub(crate) draft: Option<LongClip>,
    pub(crate) selection: Option<Range<u32>>,
    pub(crate) anchor: Option<u32>,
    pub(crate) snap: bool,
    pub(crate) panel: bool,
    /// 左侧工具栏展开的工具面板：Some(None) 为工具库全部工具。
    pub(crate) tool_palette: Option<Option<crate::toolbar_groups::ToolGroup>>,
    pub(crate) hover: Option<usize>,
    pub(crate) hover_preview: Option<crate::long_clip_chrome::LongClipHoverPreview>,
    pub(crate) trim_inputs: Option<gpui::Entity<crate::long_clip_parameters::LongClipParameters>>,
    pub(crate) parameters_pending: bool,
    pub(crate) navigation: crate::long_clip_navigation::NavigationState,
    pub(crate) piece_previews: crate::long_clip_preview::LongClipPreviews,
    pub(crate) drag: Option<ClipDrag>,
    pub(crate) space: bool,
    pub(crate) alt: bool,
    pub(crate) pointer: Option<Point>,
    pub(crate) drag_epoch: u64,
    pub(crate) selection_end: bool,
    snaps: BlankSnaps,
}

impl LongClipUi {
    pub(crate) fn new(snaps: BlankSnaps) -> Self {
        Self {
            expanded: Vec::new(),
            draft: None,
            selection: None,
            anchor: None,
            snap: true,
            panel: true,
            tool_palette: None,
            hover: None,
            hover_preview: None,
            trim_inputs: None,
            parameters_pending: false,
            navigation: Default::default(),
            piece_previews: Default::default(),
            drag: None,
            space: false,
            alt: false,
            pointer: None,
            drag_epoch: 0,
            selection_end: true,
            snaps,
        }
    }

    /// 容差由宿主从逻辑像素换算为原图像素；Alt 只暂停本次吸附。
    pub(crate) fn snap_position(&self, position: f32, tolerance: f32, alt: bool) -> u32 {
        if self.snap && !alt {
            if let Some(value) = self.snaps.nearest(position, tolerance) {
                return value;
            }
        }
        position.max(0.).round() as u32
    }

    pub(crate) fn set_selection(&mut self, anchor: u32, end: u32) {
        self.anchor = Some(anchor);
        self.selection = (anchor != end).then(|| anchor.min(end)..anchor.max(end));
        self.selection_end = end >= anchor;
    }

    pub(crate) fn retain_expanded(&mut self, clip: &LongClip) {
        self.expanded.retain(|range| clip.removed().contains(range));
        if self
            .hover
            .is_some_and(|index| index >= clip.removed().len())
        {
            self.hover = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stitch::ScrollAxis;

    #[test]
    fn long_clip_ui_selection_and_expansion_are_transient_source_ranges() {
        let mut ui = LongClipUi::new(BlankSnaps::default());
        ui.set_selection(70, 20);
        assert_eq!(ui.selection, Some(20..70));
        assert!(!ui.selection_end);
        ui.set_selection(30, 30);
        assert_eq!(ui.selection, None);
        assert_eq!(ui.anchor, Some(30));
        let mut clip = LongClip::new(100, 200, ScrollAxis::Vertical).unwrap();
        clip.delete(30..50).unwrap();
        ui.expanded = vec![30..50, 80..100];
        ui.retain_expanded(&clip);
        assert_eq!(ui.expanded, vec![30..50]);
        assert_eq!(ui.snap_position(17.6, 4., true), 18);
    }
}
