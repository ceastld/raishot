//! 当前 OCR 行的局部重识别目标与独立候选；不修改完整正文、选择或标注。
//! ROI 来自真实裁行四角。CTC 词框仅作完整性证据，不当作精确墨迹。
use crate::{
    annotation::Point,
    capture::Image,
    capture_view::ocr_task::{Receipt, TaskRequest, TaskSlot},
    geometry::Rect,
    recognition::{geometry::Projection, OcrProvider, RecognizedText, TextGeometry},
};
use std::{ops::Range, sync::Arc};
use unicode_segmentation::UnicodeSegmentation;

const MAX_SOURCE_PIXELS: u64 = 32_000_000;
const MAX_SOURCE_SIDE: u32 = 65_535;
const MAX_ROI_PIXELS: u64 = 4_000_000;
const MAX_ROI_SIDE: u32 = 8192;
const MAX_PADDING: u32 = 64;
const MAX_TEXT_BYTES: usize = 1_048_576;
const MAX_WORDS: usize = 32_768;
const MAX_LINES: usize = 4096;
const MAX_CANDIDATE_BYTES: usize = 65_536;
const MAX_ERROR_BYTES: usize = 4096;

/// 产生当前识别 DTO 的固定输入图；截图使用全局物理原点，贴图使用原图裁剪原点。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceFrame {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) origin: (i32, i32),
}

impl SourceFrame {
    fn valid(self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= MAX_SOURCE_SIDE
            && self.height <= MAX_SOURCE_SIDE
            && u64::from(self.width) * u64::from(self.height) <= MAX_SOURCE_PIXELS
            && self.origin.0.checked_add(self.width as i32).is_some()
            && self.origin.1.checked_add(self.height as i32).is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TargetError {
    Inactive,
    EditedBody,
    InvalidFrame,
    Budget,
    InvalidSelection,
    CrossLine,
    MissingLine,
    TextMismatch,
    MissingWords,
    BadGeometry,
    AmbiguousLine,
    InvalidPixels,
}

#[derive(Clone, Debug)]
pub(crate) struct LocalTarget {
    frame: SourceFrame,
    line_index: usize,
    selected: Range<usize>,
    #[cfg(test)]
    line_range: Range<usize>,
    #[cfg(test)]
    original_line: String,
    roi: Rect,
    #[cfg(test)]
    geometry: TextGeometry,
    #[cfg(test)]
    roi_geometry: TextGeometry,
}

impl LocalTarget {
    pub(crate) fn resolve(
        recognized: &RecognizedText,
        selected: Range<usize>,
        frame: SourceFrame,
        padding: u32,
    ) -> Result<Self, TargetError> {
        if !frame.valid() {
            return Err(TargetError::InvalidFrame);
        }
        if padding > MAX_PADDING
            || !within_budget(recognized, MAX_TEXT_BYTES)
            || recognized.text.split('\n').take(MAX_LINES + 1).count() > MAX_LINES
        {
            return Err(TargetError::Budget);
        }
        if selected.start >= selected.end
            || selected.end > recognized.text.len()
            || !recognized.text.is_char_boundary(selected.start)
            || !recognized.text.is_char_boundary(selected.end)
        {
            return Err(TargetError::InvalidSelection);
        }
        let mut offset = 0;
        let mut row = None;
        for (index, raw) in recognized.text.split('\n').enumerate() {
            // CRLF 是分隔符整体；不得让选择或 ROI 把 CR 当作可识别字符。
            let content = if offset + raw.len() < recognized.text.len() {
                raw.strip_suffix('\r').unwrap_or(raw)
            } else {
                raw
            };
            let range = offset..offset + content.len();
            if selected.start >= range.start && selected.end <= range.end {
                row = Some((index, range, content));
                break;
            }
            offset += raw.len() + 1;
        }
        let (line_index, line_range, content) = row.ok_or(TargetError::CrossLine)?;
        let boundaries = grapheme_boundaries(content);
        if !boundaries[selected.start - line_range.start]
            || !boundaries[selected.end - line_range.start]
        {
            return Err(TargetError::InvalidSelection);
        }
        let line = recognized
            .lines
            .get(line_index)
            .ok_or(TargetError::MissingLine)?;
        if line.text != content || content.trim().is_empty() {
            return Err(TargetError::TextMismatch);
        }
        let line_rect = line
            .rect
            .filter(|rect| rect.valid(frame.width, frame.height))
            .ok_or(TargetError::BadGeometry)?;
        // line.rect 是词框 union，不保证包含完整裁行；只在完整输入图验证投影。
        let geometry = line.geometry.ok_or(TargetError::BadGeometry)?;
        let projection = geometry
            .validated_in(Rect::full(frame.width, frame.height))
            .ok_or(TargetError::BadGeometry)?;
        let bounds = projection_bounds(projection, frame)?;
        complete_words(
            recognized,
            line_index,
            content,
            &boundaries,
            line_rect,
            projection,
            frame,
        )?;
        let right = bounds.x + bounds.width;
        let bottom = bounds.y + bounds.height;
        let x = bounds.x.saturating_sub(padding);
        let y = bounds.y.saturating_sub(padding);
        let roi = Rect {
            x,
            y,
            width: right.saturating_add(padding).min(frame.width) - x,
            height: bottom.saturating_add(padding).min(frame.height) - y,
        };
        if roi.width > MAX_ROI_SIDE
            || roi.height > MAX_ROI_SIDE
            || u64::from(roi.width) * u64::from(roi.height) > MAX_ROI_PIXELS
        {
            return Err(TargetError::Budget);
        }
        // 实际提取是轴对齐 ROI，padding 也不能悄悄把其它行带入候选。
        reject_ambiguous_line(recognized, line_index, roi, frame)?;
        let roi_geometry = TextGeometry {
            quad: geometry.quad.map(|p| Point {
                x: p.x - roi.x as f32,
                y: p.y - roi.y as f32,
            }),
        };
        roi_geometry
            .validated_in(Rect::full(roi.width, roi.height))
            .ok_or(TargetError::BadGeometry)?;
        Ok(Self {
            frame,
            line_index,
            selected,
            #[cfg(test)]
            line_range,
            #[cfg(test)]
            original_line: content.into(),
            roi,
            #[cfg(test)]
            geometry,
            #[cfg(test)]
            roi_geometry,
        })
    }

    pub(crate) fn roi(&self) -> Rect {
        self.roi
    }

    pub(crate) fn selected(&self) -> Range<usize> {
        self.selected.clone()
    }

    #[cfg(test)]
    pub(crate) fn line_range(&self) -> Range<usize> {
        self.line_range.clone()
    }

    pub(crate) fn line_index(&self) -> usize {
        self.line_index
    }

    #[cfg(test)]
    pub(crate) fn original_line(&self) -> &str {
        &self.original_line
    }

    #[cfg(test)]
    pub(crate) fn geometry(&self) -> TextGeometry {
        self.geometry
    }

    #[cfg(test)]
    pub(crate) fn roi_geometry(&self) -> TextGeometry {
        self.roi_geometry
    }

    #[cfg(test)]
    pub(crate) fn global_roi(&self) -> (i64, i64, u32, u32) {
        (
            i64::from(self.frame.origin.0) + i64::from(self.roi.x),
            i64::from(self.frame.origin.1) + i64::from(self.roi.y),
            self.roi.width,
            self.roi.height,
        )
    }

    #[cfg(test)]
    pub(crate) fn global_geometry(&self) -> [[f64; 2]; 4] {
        self.geometry.quad.map(|p| {
            [
                f64::from(self.frame.origin.0) + f64::from(p.x),
                f64::from(self.frame.origin.1) + f64::from(p.y),
            ]
        })
    }

    #[cfg(test)]
    pub(crate) fn roi_to_source(&self, p: Point) -> Option<Point> {
        (p.x.is_finite()
            && p.y.is_finite()
            && p.x >= 0.
            && p.y >= 0.
            && p.x <= self.roi.width as f32
            && p.y <= self.roi.height as f32)
            .then_some(Point {
                x: p.x + self.roi.x as f32,
                y: p.y + self.roi.y as f32,
            })
    }

    /// 只从产生 DTO 的固定输入图截取 BGRA；不得传 UI 预览或后加的标记图。
    pub(crate) fn extract_pixels(&self, image: &Image) -> Result<Image, TargetError> {
        let count = u64::from(image.width) * u64::from(image.height);
        if image.width != self.frame.width
            || image.height != self.frame.height
            || count.checked_mul(4) != Some(image.bgra.len() as u64)
        {
            return Err(TargetError::InvalidPixels);
        }
        let stride = image.width as usize * 4;
        let row_bytes = self.roi.width as usize * 4;
        let mut bgra = Vec::with_capacity(row_bytes * self.roi.height as usize);
        for y in self.roi.y..self.roi.y + self.roi.height {
            let start = y as usize * stride + self.roi.x as usize * 4;
            bgra.extend_from_slice(&image.bgra[start..start + row_bytes]);
        }
        Ok(Image {
            width: self.roi.width,
            height: self.roi.height,
            bgra,
            readback_ms: 0.,
        })
    }
}

fn grapheme_boundaries(text: &str) -> Vec<bool> {
    let mut boundaries = vec![false; text.len() + 1];
    for (byte, _) in text.grapheme_indices(true) {
        boundaries[byte] = true;
    }
    boundaries[text.len()] = true;
    boundaries
}

fn within_budget(recognized: &RecognizedText, text_limit: usize) -> bool {
    recognized.text.len() <= text_limit
        && recognized.regions.len() <= MAX_WORDS
        && recognized.words.len() <= MAX_WORDS
        && recognized.lines.len() <= MAX_LINES
        && recognized
            .words
            .iter()
            .try_fold(0usize, |sum, word| {
                sum.checked_add(word.text.len())
                    .filter(|sum| *sum <= text_limit)
            })
            .is_some()
        && recognized
            .lines
            .iter()
            .try_fold(0usize, |sum, line| {
                sum.checked_add(line.text.len())
                    .filter(|sum| *sum <= text_limit)
            })
            .is_some()
}

fn projection_bounds(projection: Projection, frame: SourceFrame) -> Result<Rect, TargetError> {
    let quad = projection.quad();
    let x = quad
        .iter()
        .map(|p| p.x)
        .fold(f32::INFINITY, f32::min)
        .floor() as u32;
    let y = quad
        .iter()
        .map(|p| p.y)
        .fold(f32::INFINITY, f32::min)
        .floor() as u32;
    let right = quad
        .iter()
        .map(|p| p.x)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as u32;
    let bottom = quad
        .iter()
        .map(|p| p.y)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil() as u32;
    let rect = Rect {
        x,
        y,
        width: right.checked_sub(x).ok_or(TargetError::BadGeometry)?,
        height: bottom.checked_sub(y).ok_or(TargetError::BadGeometry)?,
    };
    rect.valid(frame.width, frame.height)
        .then_some(rect)
        .ok_or(TargetError::BadGeometry)
}

fn contains_rect(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

fn intersects(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

fn complete_words(
    recognized: &RecognizedText,
    line_index: usize,
    content: &str,
    boundaries: &[bool],
    line_rect: Rect,
    line_projection: Projection,
    frame: SourceFrame,
) -> Result<(), TargetError> {
    let mut cursor = 0;
    let mut count = 0;
    let start = line_projection.point(0., 0.5);
    let end = line_projection.point(1., 0.5);
    let reading_span = f64::from(end.x - start.x).hypot(f64::from(end.y - start.y));
    if !reading_span.is_finite() || reading_span <= 0.0001 {
        return Err(TargetError::BadGeometry);
    }
    let coordinate = line_projection
        .quad()
        .iter()
        .map(|p| p.x.abs().max(p.y.abs()))
        .fold(1., f32::max);
    // 阅读中点须超过源 f32 投影的不确定度，最多 1e-4 个裁行 u 单元。
    let u_tolerance = ((0.0001 + f64::from(coordinate) * f64::from(f32::EPSILON) * 4.)
        / reading_span)
        .min(0.0001);
    let mut previous: Option<(f64, [Point; 4])> = None;
    for word in recognized
        .words
        .iter()
        .filter(|word| word.line == line_index)
    {
        count += 1;
        if word.text.len() > MAX_TEXT_BYTES
            || word.text.trim().is_empty()
            || !word.rect.valid(frame.width, frame.height)
            || !contains_rect(line_rect, word.rect)
            || word.horizontal_direction != recognized.lines[line_index].horizontal_direction
        {
            return Err(TargetError::MissingWords);
        }
        let geometry = word.geometry.ok_or(TargetError::BadGeometry)?;
        let projection = geometry
            .validated_in(word.rect)
            .ok_or(TargetError::BadGeometry)?;
        let quad = projection.quad();
        for p in quad {
            let uv = line_projection.inverse(p).ok_or(TargetError::BadGeometry)?;
            if uv.iter().any(|n| !(-0.00001..=1.00001).contains(n)) {
                return Err(TargetError::BadGeometry);
            }
        }
        let start = line_projection
            .inverse(projection.point(0., 0.5))
            .ok_or(TargetError::BadGeometry)?;
        let end = line_projection
            .inverse(projection.point(1., 0.5))
            .ok_or(TargetError::BadGeometry)?;
        if end[0] <= start[0] {
            return Err(TargetError::BadGeometry);
        }
        let midpoint = line_projection
            .inverse(projection.point(0.5, 0.5))
            .ok_or(TargetError::BadGeometry)?[0];
        if previous.is_some_and(|(before_midpoint, before_quad)| {
            // CTC 估计词带可以重叠，不是互斥墨迹框；只核原文阅读次序及非重合证据。
            quad == before_quad || midpoint <= before_midpoint + u_tolerance
        }) {
            return Err(TargetError::BadGeometry);
        }
        previous = Some((midpoint, quad));
        let found = content[cursor..]
            .find(&word.text)
            .ok_or(TargetError::MissingWords)?;
        let start = cursor + found;
        let end = start + word.text.len();
        if !content[cursor..start].chars().all(char::is_whitespace)
            || !boundaries[start]
            || !boundaries[end]
        {
            return Err(TargetError::MissingWords);
        }
        cursor = end;
    }
    if count == 0 || !content[cursor..].chars().all(char::is_whitespace) {
        return Err(TargetError::MissingWords);
    }
    Ok(())
}

fn reject_ambiguous_line(
    recognized: &RecognizedText,
    line_index: usize,
    bounds: Rect,
    frame: SourceFrame,
) -> Result<(), TargetError> {
    for (index, other) in recognized.lines.iter().enumerate() {
        if index == line_index {
            continue;
        }
        let other_bounds = if let Some(geometry) = other.geometry {
            let projection = geometry
                .validated_in(Rect::full(frame.width, frame.height))
                .ok_or(TargetError::BadGeometry)?;
            Some(projection_bounds(projection, frame)?)
        } else {
            other
                .rect
                .filter(|rect| rect.valid(frame.width, frame.height))
        };
        if other_bounds.is_some_and(|other| intersects(bounds, other)) {
            return Err(TargetError::AmbiguousLine);
        }
    }
    if recognized.words.iter().any(|word| {
        word.line != line_index
            && word.rect.valid(frame.width, frame.height)
            && intersects(bounds, word.rect)
    }) {
        return Err(TargetError::AmbiguousLine);
    }
    Ok(())
}

/// 身份在实际安装时生成并保留；不得每帧新建，也不得用正文 bytes 充当身份。
#[derive(Clone)]
pub(crate) struct Binding<S> {
    pub(crate) source: S,
    pub(crate) source_identity: Arc<()>,
    pub(crate) result: Arc<RecognizedText>,
    pub(crate) result_generation: u64,
    pub(crate) installed_body_identity: Arc<()>,
    pub(crate) body_identity: Arc<()>,
    pub(crate) installed_body_revision: u64,
    pub(crate) body_revision: u64,
    pub(crate) frame: SourceFrame,
    pub(crate) provider: OcrProvider,
}

impl<S: PartialEq> Binding<S> {
    fn matches(&self, current: &Self, body: &str) -> bool {
        self.source == current.source
            && Arc::ptr_eq(&self.source_identity, &current.source_identity)
            && Arc::ptr_eq(&self.result, &current.result)
            && self.result_generation == current.result_generation
            && Arc::ptr_eq(
                &self.installed_body_identity,
                &current.installed_body_identity,
            )
            && Arc::ptr_eq(&self.body_identity, &current.body_identity)
            && Arc::ptr_eq(&current.body_identity, &current.installed_body_identity)
            && self.installed_body_revision == current.installed_body_revision
            && self.body_revision == current.body_revision
            && current.body_revision == current.installed_body_revision
            && self.frame == current.frame
            && self.provider == current.provider
            && self.result.text == body
            && current.result.text == body
    }
}

struct Intent<S> {
    binding: Binding<S>,
    target: LocalTarget,
}

#[derive(Clone)]
pub(crate) struct LocalRequest<S> {
    task: TaskRequest<Arc<Intent<S>>>,
}

impl<S> LocalRequest<S> {
    pub(crate) fn target(&self) -> &LocalTarget {
        &self.task.key.target
    }

    pub(crate) fn provider(&self) -> OcrProvider {
        self.task.key.binding.provider
    }

    pub(crate) fn token(&self) -> &crate::recognition::task::TaskToken {
        &self.task.token
    }
}

pub(crate) struct Candidate<S> {
    intent: Arc<Intent<S>>,
    text: String,
}

impl<S> Candidate<S> {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    #[cfg(test)]
    pub(crate) fn target(&self) -> &LocalTarget {
        &self.intent.target
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Completion {
    Ready,
    Failed,
    Cancelled,
    Invalidated,
    Stale,
}

/// 任务取消和 Outcome 分类复用现有纯槽；本类只增加局部候选与意图资格。
pub(crate) struct LocalRetry<S> {
    tasks: TaskSlot<Arc<Intent<S>>>,
    intent: Option<Arc<Intent<S>>>,
    candidate: Option<Candidate<S>>,
    error: Option<String>,
}

impl<S> Default for LocalRetry<S> {
    fn default() -> Self {
        Self {
            tasks: TaskSlot::default(),
            intent: None,
            candidate: None,
            error: None,
        }
    }
}

impl<S: Clone + PartialEq> LocalRetry<S> {
    pub(crate) fn begin(
        &mut self,
        binding: &Binding<S>,
        body: &str,
        selected: Range<usize>,
        padding: u32,
        active: bool,
    ) -> Result<LocalRequest<S>, TargetError> {
        if !active {
            return Err(TargetError::Inactive);
        }
        if !Arc::ptr_eq(&binding.body_identity, &binding.installed_body_identity)
            || binding.body_revision != binding.installed_body_revision
            || binding.result.text != body
        {
            return Err(TargetError::EditedBody);
        }
        // 无效目标不先取消已有任务或隐藏现有候选。
        let target = LocalTarget::resolve(&binding.result, selected, binding.frame, padding)?;
        let intent = Arc::new(Intent {
            binding: binding.clone(),
            target,
        });
        let task = self.tasks.start(intent.clone());
        self.intent = Some(intent);
        self.candidate = None;
        self.error = None;
        Ok(LocalRequest { task })
    }

    fn owns(&self, request: &LocalRequest<S>) -> bool {
        self.tasks.owns(request.task.id)
            && self
                .tasks
                .key()
                .is_some_and(|key| Arc::ptr_eq(key, &request.task.key))
            && self
                .intent
                .as_ref()
                .is_some_and(|intent| Arc::ptr_eq(intent, &request.task.key))
    }

    pub(crate) fn complete(
        &mut self,
        request: &LocalRequest<S>,
        current: &Binding<S>,
        body: &str,
        active: bool,
        receipt: Receipt<RecognizedText>,
    ) -> Completion {
        // 跨窗口或旧意图不能释放当前槽，即使 id/source/generation 数字相同。
        if !self.owns(request) {
            return Completion::Stale;
        }
        if !active || !request.task.key.binding.matches(current, body) {
            self.cancel("局部 OCR 的来源、正文或活动窗口已变化");
            return Completion::Invalidated;
        }
        let receipt = self
            .tasks
            .finish_receipt(request.task.id, receipt)
            .expect("局部意图与共享任务槽已共同核验");
        match receipt {
            Receipt::Completed(result) => {
                if !within_budget(&result, MAX_CANDIDATE_BYTES) {
                    self.error = Some("局部重新识别候选超过预算".into());
                    return Completion::Failed;
                }
                if result.text.trim().is_empty() {
                    self.error = Some("局部重新识别未得到文字".into());
                    return Completion::Failed;
                }
                self.candidate = Some(Candidate {
                    intent: request.task.key.clone(),
                    text: result.text,
                });
                self.error = None;
                Completion::Ready
            }
            Receipt::Cancelled => {
                self.intent = None;
                self.error = None;
                Completion::Cancelled
            }
            Receipt::Failed(error) => {
                self.error = Some(bounded_error(error));
                Completion::Failed
            }
        }
    }

    /// 宿主在失焦、换图、换来源、结果安装、正文 revision 变化时调用。
    #[cfg(test)]
    pub(crate) fn observe(&mut self, current: &Binding<S>, body: &str, active: bool) -> bool {
        if self
            .intent
            .as_ref()
            .is_some_and(|intent| !active || !intent.binding.matches(current, body))
        {
            self.cancel("局部候选失去当前来源资格");
            true
        } else {
            false
        }
    }

    pub(crate) fn cancel(&mut self, reason: &str) -> bool {
        let had_intent = self.intent.take().is_some();
        let cancelled = self.tasks.cancel(reason);
        self.candidate = None;
        self.error = None;
        had_intent || cancelled
    }

    fn current(&self, binding: &Binding<S>, body: &str, active: bool) -> bool {
        active
            && self
                .intent
                .as_ref()
                .is_some_and(|intent| intent.binding.matches(binding, body))
    }

    /// 仅展示当前独立意图，不暴露/复制任务槽；失败后仍可从原目标显式复试。
    pub(crate) fn target(
        &self,
        binding: &Binding<S>,
        body: &str,
        active: bool,
    ) -> Option<&LocalTarget> {
        self.intent
            .as_ref()
            .filter(|_| self.current(binding, body, active))
            .map(|intent| &intent.target)
    }

    pub(crate) fn pending(&self, binding: &Binding<S>, body: &str, active: bool) -> bool {
        self.current(binding, body, active) && self.tasks.current_id().is_some()
    }

    /// 只为反馈借用实际活动槽；来源资格由宿主现有的固定输入身份门保证。
    /// 不复制 Owner，不为动画逐帧比较全文，也不授权候选发布或输出。
    pub(crate) fn pending_visual(&self) -> Option<(u64, &LocalTarget)> {
        let id = self.tasks.current_id()?;
        let intent = self.intent.as_ref()?;
        if self.tasks.key().is_none_or(|key| !Arc::ptr_eq(key, intent))
            || self.tasks.token()?.is_cancelled()
        {
            return None;
        }
        Some((id, &intent.target))
    }

    pub(crate) fn candidate(
        &self,
        binding: &Binding<S>,
        body: &str,
        active: bool,
    ) -> Option<&Candidate<S>> {
        self.candidate.as_ref().filter(|candidate| {
            self.current(binding, body, active)
                && self
                    .intent
                    .as_ref()
                    .is_some_and(|intent| Arc::ptr_eq(intent, &candidate.intent))
        })
    }

    pub(crate) fn error(&self, binding: &Binding<S>, body: &str, active: bool) -> Option<&str> {
        self.current(binding, body, active)
            .then_some(self.error.as_deref())
            .flatten()
    }

    /// 显式复制保留候选；失败原样返回，允许同一候选重试，不操作完整 OCR 会话。
    pub(crate) fn copy_candidate<E>(
        &self,
        binding: &Binding<S>,
        body: &str,
        active: bool,
        write: impl FnOnce(&str) -> Result<(), E>,
    ) -> Result<bool, E> {
        let Some(candidate) = self.candidate(binding, body, active) else {
            return Ok(false);
        };
        write(candidate.text())?;
        Ok(true)
    }
}

fn bounded_error(mut error: String) -> String {
    if error.len() > MAX_ERROR_BYTES {
        let mut end = MAX_ERROR_BYTES;
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        error.truncate(end);
    }
    error
}

#[path = "ocr_local_candidate.rs"]
pub(crate) mod candidate;

#[cfg(test)]
#[path = "ocr_local_retry_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "ocr_local_retry_actual_cpu_qa.rs"]
mod actual_cpu_qa;
