//! 局部行候选在截图与贴图两个宿主间共用的部分：候选面板与按钮快照、真实 paint 回执、
//! 滚动几何、拖选延伸和上下键导航。宿主只提供各自的就绪门、焦点、鼠标阶段与布局位置。
use crate::{
    capture::Image,
    capture_view::text_result::{
        readonly_visual_layout::from_painted_layout,
        readonly_visual_navigation::{
            move_vertical, VerticalIntent, VerticalState, VisualCache, VisualLayout,
        },
        ResultEditor,
    },
    recognition::RecognizedText,
};
use gpui::{px, rgba, Bounds, FocusHandle, Pixels, Point, ScrollHandle, TextLayout, Window};
use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc, time::Instant};

const EDGE_BAND: f64 = 24.0;
const MAX_EDGE_SPEED: f64 = 720.0;
const MAX_FRAME_SECONDS: f64 = 0.05;

pub(crate) struct CandidatePanel<S> {
    pub(crate) identity: Arc<()>,
    pub(crate) editor: ResultEditor, // 只用实际文字选择／导航方法，不注册输入器或正文写入。
    pub(crate) painted: Rc<RefCell<Option<Arc<()>>>>,
    pub(crate) visual: Rc<RefCell<VisualCache>>,
    pub(crate) vertical: VerticalState,
    pub(crate) scroll: ScrollHandle,
    pub(crate) scrolling: Rc<RefCell<S>>,
    pub(crate) _blur: Option<gpui::Subscription>,
}

impl<S: Default> CandidatePanel<S> {
    pub(crate) fn new(identity: Arc<()>, text: String, blur: Option<gpui::Subscription>) -> Self {
        Self {
            identity,
            editor: ResultEditor::new(text, 0..0),
            painted: Default::default(),
            visual: Default::default(),
            vertical: Default::default(),
            scroll: Default::default(),
            scrolling: Default::default(),
            _blur: blur,
        }
    }
}

impl<S> CandidatePanel<S> {
    /// `frame` 仍是本面板，且上一次真实 paint 正是它。
    pub(crate) fn painted_frame(&self, frame: &Arc<()>) -> bool {
        Arc::ptr_eq(frame, &self.identity)
            && self
                .painted
                .borrow()
                .as_ref()
                .is_some_and(|painted| Arc::ptr_eq(frame, painted))
    }

    /// 该 render 的视觉行仍对应当前正文、窗口尺寸与缩放。
    pub(crate) fn visual_current(&self, epoch: &Arc<()>, w: &Window) -> bool {
        let cache = self.visual.borrow();
        cache.epoch_is_current(epoch)
            && cache.current_frame(
                &self.editor.body_identity,
                self.editor.revision,
                window_size(w),
                w.scale_factor(),
            )
    }

    /// 按当前帧视觉行上下移动并写入只读选择；没有可用停点时不动，返回新的活动端。
    pub(crate) fn move_vertical(&mut self, down: bool, shift: bool, w: &Window) -> Option<usize> {
        let cache = self.visual.borrow();
        let layout = cache.current(
            &self.editor.body_identity,
            self.editor.revision,
            window_size(w),
            w.scale_factor(),
        );
        let (anchor, caret) = move_vertical(
            &self.editor.text,
            layout,
            true,
            VerticalIntent {
                selection: self.editor.input.selection.clone(),
                reversed: self.editor.input.reversed,
                down,
                shift,
            },
            &mut self.vertical,
        )?;
        drop(cache);
        self.editor.readonly_set_selection(anchor, caret);
        Some(caret)
    }

    pub(crate) fn styled_text(&self) -> gpui::StyledText {
        let range = self.editor.input.selection.clone();
        gpui::StyledText::new(self.editor.text.clone()).with_highlights(
            (!range.is_empty()).then_some((
                range,
                gpui::HighlightStyle {
                    background_color: Some(rgba(0x5796ff66).into()),
                    ..Default::default()
                },
            )),
        )
    }

    pub(crate) fn button_stamp<T>(
        &self,
        source_identity: &Arc<()>,
        source: T,
        generation: u64,
    ) -> CandidateButtonStamp<T> {
        CandidateButtonStamp {
            frame: Arc::clone(&self.identity),
            body_identity: Arc::clone(&self.editor.body_identity),
            revision: self.editor.revision,
            selection: self.editor.input.selection.clone(),
            reversed: self.editor.input.reversed,
            source_identity: Arc::clone(source_identity),
            source,
            generation,
        }
    }

    /// render 时固定本次 paint 要核对的正文与布局。
    pub(crate) fn paint_stamp(&self, epoch: Arc<()>, layout: TextLayout) -> CandidatePaint {
        CandidatePaint {
            epoch,
            layout,
            body: self.editor.text.clone(),
            body_identity: Arc::clone(&self.editor.body_identity),
            revision: self.editor.revision,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateCommandKind {
    Copy,
    Retry,
}

#[derive(Clone)]
pub(crate) struct CandidateButtonStamp<S> {
    pub(crate) frame: Arc<()>,
    pub(crate) body_identity: Arc<()>,
    pub(crate) revision: u64,
    pub(crate) selection: Range<usize>,
    pub(crate) reversed: bool,
    pub(crate) source_identity: Arc<()>,
    pub(crate) source: S,
    pub(crate) generation: u64,
}

impl<S: PartialEq> CandidateButtonStamp<S> {
    /// 来源、结果代、候选正文与选择都未变，且没有拖选或输入法组合。
    pub(crate) fn matches<T>(
        &self,
        panel: &CandidatePanel<T>,
        source_identity: &Arc<()>,
        source: &S,
        generation: u64,
    ) -> bool {
        Arc::ptr_eq(source_identity, &self.source_identity)
            && *source == self.source
            && generation == self.generation
            && Arc::ptr_eq(&panel.identity, &self.frame)
            && Arc::ptr_eq(&panel.editor.body_identity, &self.body_identity)
            && panel.editor.revision == self.revision
            && panel.editor.input.selection == self.selection
            && panel.editor.input.reversed == self.reversed
            && !panel.editor.dragging()
            && !panel.editor.composing()
    }
}

pub(crate) struct CandidateCommand<S> {
    pub(crate) token: Arc<()>,
    pub(crate) kind: CandidateCommandKind,
    pub(crate) stamp: CandidateButtonStamp<S>,
    pub(crate) focus: FocusHandle,
    pub(crate) _blur: gpui::Subscription,
    pub(crate) scheduled: bool,
}

pub(crate) struct CandidateScrollDrag {
    pub(crate) pointer: Point<Pixels>,
    pub(crate) last_tick: Instant,
}

/// 一次当前真实 paint 的滚动回执；`rows` 由宿主决定是否携带行停点。
#[derive(Clone)]
pub(crate) struct CandidateScrollPaint<R = ()> {
    pub(crate) epoch: Arc<()>,
    pub(crate) layout: TextLayout,
    pub(crate) rows: R,
    pub(crate) viewport: Bounds<Pixels>,
    pub(crate) offset: Point<Pixels>,
    pub(crate) window_size: [f32; 2],
    pub(crate) scale: f32,
}

impl<R> CandidateScrollPaint<R> {
    /// 回执仍对应当前候选正文、窗口尺寸、缩放与滚动位置。
    pub(crate) fn current<S>(&self, panel: &CandidatePanel<S>, w: &Window) -> bool {
        let visual = panel.visual.borrow();
        visual.epoch_is_current(&self.epoch)
            && visual.current_frame(
                &panel.editor.body_identity,
                panel.editor.revision,
                self.window_size,
                self.scale,
            )
            && self.window_size == window_size(w)
            && self.scale == w.scale_factor()
            && self.viewport == panel.scroll.bounds()
            && self.offset == panel.scroll.offset()
    }
}

pub(crate) struct CandidatePaint {
    pub(crate) epoch: Arc<()>,
    pub(crate) layout: TextLayout,
    body: String,
    body_identity: Arc<()>,
    revision: u64,
}

impl CandidatePaint {
    pub(crate) fn same_body(&self, editor: &ResultEditor) -> bool {
        Arc::ptr_eq(&editor.body_identity, &self.body_identity) && editor.revision == self.revision
    }

    /// 发布本次真实 paint：当前时安装视觉行（无法提取则只保留帧身份），否则作废缓存。
    /// `None` 表示已有后继 render，过期 paint 不得改写它；`Some(None)` 表示本次不当前。
    pub(crate) fn publish<R>(
        &self,
        visual: &RefCell<VisualCache>,
        current: bool,
        scroll: &ScrollHandle,
        w: &Window,
        rows: impl FnOnce(Option<&VisualLayout>, &TextLayout) -> R,
    ) -> Option<Option<CandidateScrollPaint<R>>> {
        let layout = current
            .then(|| from_painted_layout(&self.body, &self.layout))
            .flatten();
        let rows = rows(layout.as_ref(), &self.layout);
        let mut cache = visual.borrow_mut();
        if !cache.epoch_is_current(&self.epoch) {
            return None;
        }
        let size = window_size(w);
        if let Some(layout) = layout {
            cache.publish(
                &self.epoch,
                &self.body_identity,
                self.revision,
                size,
                w.scale_factor(),
                layout,
            );
        } else if current {
            cache.publish_without_rows(
                &self.epoch,
                &self.body_identity,
                self.revision,
                size,
                w.scale_factor(),
            );
        } else {
            cache.invalidate();
        }
        Some(current.then(|| CandidateScrollPaint {
            epoch: Arc::clone(&self.epoch),
            layout: self.layout.clone(),
            rows,
            viewport: scroll.bounds(),
            offset: scroll.offset(),
            window_size: size,
            scale: w.scale_factor(),
        }))
    }
}

pub(crate) fn window_size(w: &Window) -> [f32; 2] {
    [
        f32::from(w.viewport_size().width),
        f32::from(w.viewport_size().height),
    ]
}

/// 拖选命中夹在视口内；底边留 0.01 px，避免命中视口外的下一行。
pub(crate) fn clamp_to_viewport(
    position: Point<Pixels>,
    viewport: Bounds<Pixels>,
) -> Point<Pixels> {
    let top = f32::from(viewport.top());
    gpui::point(
        px(f32::from(position.x).clamp(f32::from(viewport.left()), f32::from(viewport.right()))),
        px(f32::from(position.y).clamp(top, (f32::from(viewport.bottom()) - 0.01).max(top))),
    )
}

/// 用当前真实布局把拖选活动端延伸到指针处，再按边缘停留时间给出新的滚动 offset。
/// 返回（选择是否改变，新 offset）。
pub(crate) fn drag_tick(
    editor: &mut ResultEditor,
    drag: &mut CandidateScrollDrag,
    layout: &TextLayout,
    viewport: Bounds<Pixels>,
    offset_y: f32,
    max_scroll_y: f32,
) -> (bool, Option<f32>) {
    let index = layout
        .index_for_position(clamp_to_viewport(drag.pointer, viewport))
        .unwrap_or_else(|index| index)
        .min(editor.text.len());
    let before = (editor.input.selection.clone(), editor.input.reversed);
    editor.readonly_extend_drag(index);
    let changed = before != (editor.input.selection.clone(), editor.input.reversed);
    let now = Instant::now();
    let elapsed_seconds = now.duration_since(drag.last_tick).as_secs_f32();
    drag.last_tick = now;
    let offset = edge_scroll(EdgeScroll {
        viewport_top: f32::from(viewport.top()),
        viewport_bottom: f32::from(viewport.bottom()),
        offset_y,
        max_scroll_y,
        pointer_y: f32::from(drag.pointer.y),
        elapsed_seconds,
    });
    (changed, offset)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EdgeScroll {
    pub(crate) viewport_top: f32,
    pub(crate) viewport_bottom: f32,
    pub(crate) offset_y: f32,
    pub(crate) max_scroll_y: f32,
    pub(crate) pointer_y: f32,
    pub(crate) elapsed_seconds: f32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RevealScroll {
    pub(crate) viewport_top: f32,
    pub(crate) viewport_bottom: f32,
    pub(crate) offset_y: f32,
    pub(crate) max_scroll_y: f32,
    pub(crate) caret_top: f32,
    pub(crate) caret_bottom: f32,
}

pub(crate) fn valid_scroll(top: f32, bottom: f32, offset: f32, max_scroll: f32) -> bool {
    [top, bottom, offset, max_scroll]
        .into_iter()
        .all(f32::is_finite)
        && bottom > top
        && max_scroll >= 0.0
        && offset >= -max_scroll
        && offset <= 0.0
}

/// 返回 GPUI 负 y offset；边缘越深入滚动越快，视口外封顶。
/// 小视口将边缘带缩至半高，避免上下滚动区重叠。
pub(crate) fn edge_scroll(input: EdgeScroll) -> Option<f32> {
    if !valid_scroll(
        input.viewport_top,
        input.viewport_bottom,
        input.offset_y,
        input.max_scroll_y,
    ) || !input.pointer_y.is_finite()
        || !input.elapsed_seconds.is_finite()
        || input.elapsed_seconds <= 0.0
    {
        return None;
    }
    let top = f64::from(input.viewport_top);
    let bottom = f64::from(input.viewport_bottom);
    let pointer = f64::from(input.pointer_y);
    let band = EDGE_BAND.min((bottom - top) / 2.0);
    let direction = if pointer < top + band {
        ((top + band - pointer) / band).min(1.0)
    } else if pointer > bottom - band {
        -((pointer - (bottom - band)) / band).min(1.0)
    } else {
        return None;
    };
    let step = direction * MAX_EDGE_SPEED * f64::from(input.elapsed_seconds).min(MAX_FRAME_SECONDS);
    let offset = f64::from(input.offset_y);
    let target = (offset + step).clamp(-f64::from(input.max_scroll_y), 0.0);
    let mut next = target as f32;
    // 向原 offset 收一格，防止 f32 舍入把严格的单帧上限放大。
    if (f64::from(next) - offset).abs() > step.abs() {
        next = if next < input.offset_y {
            next.next_up()
        } else {
            next.next_down()
        };
    }
    (next != input.offset_y).then_some(next)
}

/// 使用活动端在已绘制布局中的矩形，计算最小可见偏移。
/// 一行高于视口时顶对齐；内容首尾仍服从实际可滚动范围。
pub(crate) fn reveal_scroll(input: RevealScroll) -> Option<f32> {
    if !valid_scroll(
        input.viewport_top,
        input.viewport_bottom,
        input.offset_y,
        input.max_scroll_y,
    ) || !input.caret_top.is_finite()
        || !input.caret_bottom.is_finite()
        || input.caret_bottom < input.caret_top
    {
        return None;
    }
    let top = f64::from(input.viewport_top);
    let bottom = f64::from(input.viewport_bottom);
    let caret_top = f64::from(input.caret_top);
    let caret_bottom = f64::from(input.caret_bottom);
    let adjustment = if caret_bottom - caret_top > bottom - top || caret_top < top {
        top - caret_top
    } else if caret_bottom > bottom {
        bottom - caret_bottom
    } else {
        return None;
    };
    let next =
        (f64::from(input.offset_y) + adjustment).clamp(-f64::from(input.max_scroll_y), 0.0) as f32;
    (next != input.offset_y).then_some(next)
}

/// 复制选中部分；空选择复制整条候选。
pub(crate) fn candidate_payload(text: &str, range: Range<usize>) -> &str {
    text.get(range)
        .filter(|part| !part.is_empty())
        .unwrap_or(text)
}

pub(crate) fn copy_notice<E: std::fmt::Display>(result: Result<bool, E>, copied: &str) -> String {
    match result {
        Ok(true) => copied.into(),
        Ok(false) => tr!("候选已失效，请重试", "Candidate expired; retry").into(),
        Err(error) => {
            eprintln!("局部候选复制失败：{error}");
            tr!(
                "复制失败；候选已保留，可再次 Ctrl+C",
                "Copy failed; candidate kept, retry Ctrl+C"
            )
            .into()
        }
    }
}

/// 固定输入只保留与原 OCR 调用同尺寸、在预算内的实际 BGRA；唯一一次复制发生在交给 worker 前。
pub(crate) fn retained_image(image: &Image, expected: (u32, u32)) -> Option<Arc<Image>> {
    let pixels = u64::from(image.width).checked_mul(u64::from(image.height))?;
    if image.width == 0
        || image.height == 0
        || image.width > 65_535
        || image.height > 65_535
        || (image.width, image.height) != expected
        || pixels > 32_000_000
        || pixels.checked_mul(4)? != image.bgra.len() as u64
    {
        return None;
    }
    Some(Arc::new(Image {
        width: image.width,
        height: image.height,
        bgra: image.bgra.clone(),
        readback_ms: image.readback_ms,
    }))
}

pub(crate) fn retained_result(result: &RecognizedText) -> bool {
    result.text.len() <= 1_048_576
        && result.words.len() <= 32_768
        && result.regions.len() <= 32_768
        && result.lines.len() <= 4096
        && result
            .words
            .iter()
            .map(|w| w.text.len())
            .chain(result.lines.iter().map(|l| l.text.len()))
            .try_fold(0usize, |n, len| n.checked_add(len))
            .is_some_and(|n| n <= 1_048_576)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(
        top: f32,
        bottom: f32,
        offset_y: f32,
        max: f32,
        pointer_y: f32,
        dt: f32,
    ) -> Option<f32> {
        edge_scroll(EdgeScroll {
            viewport_top: top,
            viewport_bottom: bottom,
            offset_y,
            max_scroll_y: max,
            pointer_y,
            elapsed_seconds: dt,
        })
    }

    fn reveal(top: f32, bottom: f32, offset_y: f32, max: f32, caret: (f32, f32)) -> Option<f32> {
        reveal_scroll(RevealScroll {
            viewport_top: top,
            viewport_bottom: bottom,
            offset_y,
            max_scroll_y: max,
            caret_top: caret.0,
            caret_bottom: caret.1,
        })
    }

    #[test]
    fn candidate_edge_scroll_is_bounded_and_uses_negative_actual_offset() {
        assert!(edge(20., 140., -100., 1000., 80., 0.016).is_none());
        assert_eq!(edge(20., 140., -100., 1000., 10_000., 10.), Some(-136.));
        assert_eq!(edge(20., 140., -100., 1000., -10_000., 10.), Some(-64.));
        assert_eq!(edge(20., 140., -990., 1000., 150., 1.), Some(-1000.));
        assert!(edge(20., 140., -1000., 1000., 150., 1.).is_none());
        assert!(edge(20., 140., 0., 1000., 10., 1.).is_none());
        assert!(edge(10., 12., -40., 100., 11., 0.016).is_none());
        let precise = edge(20., 140., -20_000_000., 40_000_000., 131., 0.016).unwrap();
        assert!((f64::from(precise) + 20_000_000.).abs() <= 720. * 0.016 * (15. / 24.));
        for input in [f32::NAN, f32::INFINITY] {
            assert!(edge(input, 140., -100., 1000., 150., 1.).is_none());
            assert!(edge(20., 140., -100., 1000., input, 1.).is_none());
            assert!(edge(20., 140., -100., 1000., 150., input).is_none());
        }
        assert!(edge(20., 20., -100., 1000., 150., 1.).is_none());
        assert!(edge(20., 140., -1001., 1000., 150., 1.).is_none());
        assert!(edge(20., 140., -100., 1000., 150., 0.).is_none());
    }

    #[test]
    fn candidate_caret_reveal_uses_minimal_visible_span_and_content_limits() {
        assert!(reveal(20., 140., -100., 1000., (40., 52.)).is_none());
        assert_eq!(reveal(20., 140., -100., 1000., (5., 17.)), Some(-85.));
        assert_eq!(reveal(20., 140., -100., 1000., (145., 157.)), Some(-117.));
        // 完整共享软边界的相邻两行同时保留在视口，仍只滚动所需距离。
        assert_eq!(reveal(20., 140., -100., 1000., (128., 152.)), Some(-112.));
        assert_eq!(reveal(20., 140., -990., 1000., (145., 157.)), Some(-1000.));
        assert_eq!(reveal(20., 140., -5., 1000., (5., 17.)), Some(0.));
        assert_eq!(reveal(20., 30., -100., 1000., (40., 64.)), Some(-120.));
        assert!(reveal(20., 140., -100., 1000., (70., 60.)).is_none());
        assert!(reveal(20., 140., -100., -1., (70., 80.)).is_none());
        assert!(reveal(20., 140., -100., 1000., (f32::NAN, 80.)).is_none());
    }
}
