//! 只读 OCR 的已呈现视觉行停点；不推导逻辑行，也不修改正文或编辑历史。
use super::readonly_navigation::floor_boundary;
use std::ops::Range;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VisualStop {
    pub byte: usize,
    pub x: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct VisualLine {
    pub stops: Vec<VisualStop>,
}

#[derive(Debug, PartialEq)]
struct VisualGeometry {
    text: String,
    lines: Vec<VisualLine>,
}

#[derive(Clone, Debug)]
pub(crate) struct VisualLayout {
    geometry: Arc<VisualGeometry>,
}

impl VisualLayout {
    /// 借用已经验证的视觉行停点，不扫描布局或修改正文。
    pub(crate) fn lines(&self) -> &[VisualLine] {
        &self.geometry.lines
    }

    /// x 是同一正文坐标系中的位置；统一减去首停点，面板平移不改变导航。
    /// 软折行可以在相邻两行重复 byte；硬换行只能留下完整换行字素的 gap。
    pub(crate) fn new(text: &str, mut lines: Vec<VisualLine>) -> Option<Self> {
        let origin = lines.first()?.stops.first()?.x;
        if !origin.is_finite()
            || lines.last()?.stops.last()?.byte != text.len()
            || lines.first()?.stops.first()?.byte != 0
        {
            return None;
        }
        let boundaries: Vec<_> = text
            .grapheme_indices(true)
            .map(|(byte, _)| byte)
            .chain(std::iter::once(text.len()))
            .collect();
        let mut previous_line_end = None;
        for line in &mut lines {
            let first = line.stops.first()?.byte;
            if let Some(previous) = previous_line_end {
                if first < previous {
                    return None;
                }
                if first > previous
                    && !matches!(
                        text.get(previous..first)?,
                        "\r\n" | "\r" | "\n" | "\u{0085}" | "\u{2028}" | "\u{2029}"
                    )
                {
                    return None;
                }
            }
            let mut previous_byte = None;
            for stop in &mut line.stops {
                if stop.byte > text.len()
                    || boundaries.binary_search(&stop.byte).is_err()
                    || previous_byte.is_some_and(|previous| stop.byte <= previous)
                    || !stop.x.is_finite()
                {
                    return None;
                }
                stop.x -= origin;
                if !stop.x.is_finite() {
                    return None;
                }
                previous_byte = Some(stop.byte);
            }
            previous_line_end = line.stops.last().map(|stop| stop.byte);
        }
        Some(Self {
            geometry: Arc::new(VisualGeometry {
                text: text.to_owned(),
                lines,
            }),
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct VerticalState {
    preferred_x: Option<f32>,
    geometry: Option<Arc<VisualGeometry>>,
    destination_row: Option<usize>,
    destination_caret: Option<usize>,
}

impl VerticalState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug)]
pub(crate) struct VerticalIntent {
    pub selection: Range<usize>,
    pub reversed: bool,
    pub down: bool,
    pub shift: bool,
}

/// 只有当前有效的实际视觉布局可以导航；拒绝时清掉横向偏好和行归属。
/// 无已有行归属的共享 byte 属于后一个视觉行，后续移动保留目的行归属。
/// intent 只描述当前只读选择和移动方向，不修改正文或编辑状态。
pub(crate) fn move_vertical(
    text: &str,
    layout: Option<&VisualLayout>,
    current: bool,
    intent: VerticalIntent,
    state: &mut VerticalState,
) -> Option<(usize, usize)> {
    let VerticalIntent {
        selection: range,
        reversed,
        down,
        shift,
    } = intent;
    let Some(layout) = layout.filter(|layout| current && layout.geometry.text == text) else {
        state.reset();
        return None;
    };
    let geometry = &layout.geometry;
    let same_geometry = state.geometry.as_ref().is_some_and(|previous| {
        Arc::ptr_eq(previous, geometry) || previous.as_ref() == geometry.as_ref()
    });
    if !same_geometry {
        state.reset();
    }
    let first = floor_boundary(text, range.start);
    let last = floor_boundary(text, range.end);
    let (start, end) = (first.min(last), first.max(last));
    let (anchor, caret) = if reversed { (end, start) } else { (start, end) };
    let retained_row = state
        .destination_row
        .filter(|_| state.destination_caret == Some(caret))
        .filter(|row| {
            geometry
                .lines
                .get(*row)
                .is_some_and(|line| line.stops.iter().any(|stop| stop.byte == caret))
        });
    if retained_row.is_none() {
        state.preferred_x = None;
    }
    let row = retained_row.or_else(|| {
        geometry
            .lines
            .iter()
            .rposition(|line| line.stops.iter().any(|stop| stop.byte == caret))
    });
    let Some(row) = row else {
        state.reset();
        return None;
    };
    let current_stop = geometry.lines[row]
        .stops
        .iter()
        .find(|stop| stop.byte == caret)?;
    let preferred_x = *state.preferred_x.get_or_insert(current_stop.x);
    let destination_row = if down {
        (row + 1 < geometry.lines.len()).then_some(row + 1)
    } else {
        row.checked_sub(1)
    };
    let (destination_row, next) = destination_row.map_or((row, caret), |next_row| {
        let next = geometry.lines[next_row]
            .stops
            .iter()
            .min_by(|left, right| {
                (left.x - preferred_x)
                    .abs()
                    .total_cmp(&(right.x - preferred_x).abs())
                    .then_with(|| left.byte.cmp(&right.byte))
            })
            .expect("已验证视觉行至少包含一个停点")
            .byte;
        (next_row, next)
    });
    state.geometry = Some(Arc::clone(geometry));
    state.destination_row = Some(destination_row);
    state.destination_caret = Some(next);
    Some((if shift { anchor } else { next }, next))
}

#[derive(Debug)]
struct CachedVisualLayout {
    body_identity: Arc<()>,
    revision: u64,
    viewport: [f32; 2],
    scale: f32,
    layout: Option<VisualLayout>,
}

#[derive(Debug, Default)]
pub(crate) struct VisualCache {
    epoch: Option<Arc<()>>,
    painted: Option<CachedVisualLayout>,
}

impl VisualCache {
    /// 新一次 render 尚无当前帧停点，旧 paint 回调不能发布到这次 render。
    pub(crate) fn begin_render(&mut self) -> Arc<()> {
        let epoch = Arc::new(());
        self.epoch = Some(Arc::clone(&epoch));
        self.painted = None;
        epoch
    }

    pub(crate) fn epoch_is_current(&self, epoch: &Arc<()>) -> bool {
        self.epoch
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, epoch))
    }

    /// 同时作废 epoch，失活后的旧 paint 回调也不能把布局复活。
    pub(crate) fn invalidate(&mut self) {
        self.epoch = None;
        self.painted = None;
    }

    pub(crate) fn publish(
        &mut self,
        epoch: &Arc<()>,
        body_identity: &Arc<()>,
        revision: u64,
        viewport: [f32; 2],
        scale: f32,
        layout: VisualLayout,
    ) -> bool {
        self.publish_frame(
            epoch,
            body_identity,
            revision,
            viewport,
            scale,
            Some(layout),
        )
    }

    /// 真实 paint 已完成，但当前折行不能形成完整字素停点时仅保留帧身份。
    pub(crate) fn publish_without_rows(
        &mut self,
        epoch: &Arc<()>,
        body_identity: &Arc<()>,
        revision: u64,
        viewport: [f32; 2],
        scale: f32,
    ) -> bool {
        self.publish_frame(epoch, body_identity, revision, viewport, scale, None)
    }

    fn publish_frame(
        &mut self,
        epoch: &Arc<()>,
        body_identity: &Arc<()>,
        revision: u64,
        viewport: [f32; 2],
        scale: f32,
        layout: Option<VisualLayout>,
    ) -> bool {
        if !self.epoch_is_current(epoch) {
            return false;
        }
        if !valid_viewport(viewport, scale) {
            self.painted = None;
            return false;
        }
        self.painted = Some(CachedVisualLayout {
            body_identity: Arc::clone(body_identity),
            revision,
            viewport,
            scale,
            layout,
        });
        true
    }

    pub(crate) fn current(
        &self,
        body_identity: &Arc<()>,
        revision: u64,
        viewport: [f32; 2],
        scale: f32,
    ) -> Option<&VisualLayout> {
        if !self.current_frame(body_identity, revision, viewport, scale) {
            return None;
        }
        self.painted.as_ref()?.layout.as_ref()
    }

    pub(crate) fn current_frame(
        &self,
        body_identity: &Arc<()>,
        revision: u64,
        viewport: [f32; 2],
        scale: f32,
    ) -> bool {
        let Some(painted) = self.painted.as_ref() else {
            return false;
        };
        self.epoch.is_some()
            && Arc::ptr_eq(&painted.body_identity, body_identity)
            && painted.revision == revision
            && painted.viewport == viewport
            && painted.scale == scale
            && valid_viewport(viewport, scale)
    }
}

fn valid_viewport(viewport: [f32; 2], scale: f32) -> bool {
    viewport
        .into_iter()
        .all(|size| size.is_finite() && size > 0.0)
        && scale.is_finite()
        && scale > 0.0
}
