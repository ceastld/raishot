//! 普通截图双击的纯决策；文字和折线的专用双击由调用方优先处理。
use crate::{
    annotation::{Point, Tool},
    editor::Editor,
    geometry::Rect,
};
use std::time::{Duration, Instant};

/// 已完成的首击；原生交接与普通画布点击共享既有控件/菜单失效屏障。
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClickSeed {
    point: (i32, i32),
    down_at: Instant,
    interval: Duration,
    tolerance: (i32, i32),
    origin: ClickOrigin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClickOrigin {
    Native,
    Canvas,
}

impl ClickSeed {
    pub(crate) fn new(
        point: (i32, i32),
        down_at: Instant,
        interval: Duration,
        tolerance: (i32, i32),
    ) -> Self {
        Self {
            point,
            down_at,
            interval,
            tolerance,
            origin: ClickOrigin::Native,
        }
    }

    pub(crate) fn native(self) -> bool {
        self.origin == ClickOrigin::Native
    }

    fn matches(self, point: (i32, i32), at: Instant) -> bool {
        at.checked_duration_since(self.down_at)
            .is_some_and(|elapsed| elapsed < self.interval)
            && (i64::from(point.0) - i64::from(self.point.0)).abs() <= i64::from(self.tolerance.0)
            && (i64::from(point.1) - i64::from(self.point.1)).abs() <= i64::from(self.tolerance.1)
    }

    /// 文字/折线专用分支只保留原生交接升级，不使用普通画布序列改写 raw 次数。
    pub(crate) fn click_count(self, actual_count: usize, point: (i32, i32), at: Instant) -> usize {
        if self.native() && actual_count == 1 && self.matches(point, at) {
            2
        } else {
            actual_count
        }
    }
}

/// 重放当前 LeftDown 的原始时空与已经消费的首击证明；dispatch 完成后必须清除。
#[derive(Clone, Copy, Debug)]
pub(crate) struct CanvasClickReplay {
    pub(crate) seed: Option<ClickSeed>,
    pub(crate) physical_point: (i32, i32),
    pub(crate) received_at: Instant,
}

/// 尚未抬起的已接受画布 Down；拖出后返回原点仍不能成为双击首击。
#[derive(Clone, Copy, Debug)]
pub(crate) struct CanvasClickDown {
    seed: ClickSeed,
    dragged: bool,
}

impl CanvasClickDown {
    pub(crate) fn new(
        point: (i32, i32),
        at: Instant,
        interval: Duration,
        tolerance: (i32, i32),
    ) -> Self {
        Self {
            seed: ClickSeed {
                point,
                down_at: at,
                interval,
                tolerance,
                origin: ClickOrigin::Canvas,
            },
            dragged: false,
        }
    }

    pub(crate) fn moved(&mut self, point: (i32, i32), plain_left: bool) {
        if !plain_left
            || (i64::from(point.0) - i64::from(self.seed.point.0)).abs() >= 3
            || (i64::from(point.1) - i64::from(self.seed.point.1)).abs() >= 3
        {
            self.dragged = true;
        }
    }

    pub(crate) fn finish(
        mut self,
        point: (i32, i32),
        accepted_plain_up: bool,
    ) -> Option<ClickSeed> {
        self.moved(point, accepted_plain_up);
        (!self.dragged).then_some(self.seed)
    }
}

/// 普通宿主点击链。消费事件重启下一次画布首击；正常 double 完成不重启 raw3/4。
#[derive(Default, Debug)]
pub(crate) struct CanvasClickSequence {
    pending: Option<CanvasClickDown>,
    restart: bool,
}

/// 本次真实画布 Up 的宿主资格；不保存候选、对象或回放状态。
#[derive(Clone, Copy, Debug)]
pub(crate) struct CanvasClickUp {
    pub(crate) screenshot: bool,
    pub(crate) modified: bool,
    pub(crate) busy: bool,
    pub(crate) input_active: bool,
    pub(crate) owned: bool,
}

impl CanvasClickSequence {
    pub(crate) fn consumed(&mut self) {
        self.pending = None;
        self.restart = true;
    }

    pub(crate) fn after_double(&mut self) {
        self.pending = None;
        self.restart = false;
    }

    /// 仅供普通确认/退出绘图与 count==1 的内侧/中央入口；专用文本/折线仍用 raw。
    pub(crate) fn count(
        &mut self,
        seed: Option<ClickSeed>,
        actual_count: usize,
        point: (i32, i32),
        at: Instant,
    ) -> usize {
        self.pending = None;
        let restart = std::mem::take(&mut self.restart);
        if actual_count == 0 {
            return 0;
        }
        if let Some(seed) = seed.filter(|seed| seed.matches(point, at)) {
            match seed.origin {
                ClickOrigin::Native if matches!(actual_count, 1 | 2) => return 2,
                ClickOrigin::Canvas => return 2,
                _ => {}
            }
        }
        if restart || actual_count == 2 {
            1
        } else {
            actual_count
        }
    }

    pub(crate) fn begin(
        &mut self,
        count: usize,
        ready: bool,
        point: (i32, i32),
        at: Instant,
        interval: Duration,
        tolerance: (i32, i32),
    ) {
        if !ready {
            self.consumed();
            return;
        }
        self.pending = (count == 1).then(|| CanvasClickDown::new(point, at, interval, tolerance));
    }

    pub(crate) fn moved(&mut self, point: (i32, i32), plain_left: bool) {
        if let Some(click) = &mut self.pending {
            click.moved(point, plain_left);
        }
    }

    pub(crate) fn modifiers_changed(&mut self, modified: bool) {
        if modified {
            if let Some(click) = &mut self.pending {
                click.dragged = true;
            }
        }
    }

    pub(crate) fn take_pending(&mut self) -> Option<CanvasClickDown> {
        let pending = self.pending.take();
        // 未完成的 Up 默认作废；只有调用方完成真实画布 Up 后才能恢复首击。
        if pending.is_some() {
            self.restart = true;
        }
        pending
    }

    pub(crate) fn finish(
        &mut self,
        pending: CanvasClickDown,
        point: (i32, i32),
        accepted_plain_up: bool,
    ) -> Option<ClickSeed> {
        let seed = pending.finish(point, accepted_plain_up);
        self.restart = seed.is_none();
        seed
    }

    pub(crate) fn finish_ordinary(
        &mut self,
        pending: CanvasClickDown,
        editor: &Editor,
        canvas: Rect,
        point: (i32, i32),
        up: CanvasClickUp,
    ) -> Option<ClickSeed> {
        self.finish(
            pending,
            point,
            up.owned
                && editor
                    .crop
                    .is_some_and(|crop| crop.valid(canvas.width, canvas.height))
                && ordinary_canvas_click_ready(
                    editor,
                    up.screenshot,
                    up.modified,
                    up.busy,
                    up.input_active,
                ),
        )
    }
}

/// 首击允许从无 crop 开始；完成后有效 crop 仍由原来的普通双击决策检查。
pub(crate) fn ordinary_canvas_click_ready(
    editor: &Editor,
    screenshot: bool,
    modified: bool,
    interaction_busy: bool,
    input_active: bool,
) -> bool {
    screenshot
        && !modified
        && !interaction_busy
        && !input_active
        && !editor.is_dragging()
        && editor.text_draft.is_none()
        && !editor.has_polyline()
}

/// 普通截图内的文字空白首击只结束已有编辑，不在同次 Down 新建空框。
/// 文字内区、控制点和其它已有文字的命中仍交给原来的专用输入路线。
pub(crate) fn finish_ordinary_text_blank_down(
    editor: &mut Editor,
    point: Point,
    tolerance: f32,
    canvas: Rect,
    down: CanvasClickUp,
) -> bool {
    if !down.owned
        || !down.screenshot
        || down.modified
        || down.busy
        || down.input_active
        || editor.tool != Tool::Text
        || editor.is_dragging()
        || editor.has_pending_drawing()
        || editor.lasso_draft.is_some()
        || !canvas.contains(point.rounded())
        || editor.crop.is_none_or(|crop| {
            !crop.valid(canvas.width, canvas.height) || !crop.contains(point.rounded())
        })
    {
        return false;
    }
    let Some(draft) = &editor.text_draft else {
        return false;
    };
    if draft.annotation.tool != Tool::Text
        || (!draft.annotation.valid() && !draft.annotation.text.trim().is_empty())
        || draft.annotation.hit(point, tolerance, true)
        || editor.document.items().iter().any(|item| {
            Some(item.id) != draft.id
                && item.annotation.tool.text()
                && item.annotation.hit(point, tolerance, true)
        })
    {
        return false;
    }
    editor.finish_text();
    editor.text_draft.is_none()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DoubleClickAction {
    None,
    Confirm,
    Edit,
}

impl DoubleClickAction {
    /// 只有已完成首击证明与普通宿主门认领的 double 才越过选区/候选 Down。
    pub(crate) fn starts_pointer(self) -> bool {
        self == Self::None
    }
}

pub(crate) fn ordinary_double_click(
    editor: &Editor,
    canvas: Rect,
    click_count: usize,
    screenshot: bool,
    modified: bool,
    interaction_busy: bool,
    input_active: bool,
) -> DoubleClickAction {
    if click_count != 2
        || !ordinary_canvas_click_ready(
            editor,
            screenshot,
            modified,
            interaction_busy,
            input_active,
        )
        || editor
            .crop
            .is_none_or(|crop| !crop.valid(canvas.width, canvas.height))
    {
        return DoubleClickAction::None;
    }
    if matches!(editor.tool, Tool::Crop | Tool::Edit) {
        DoubleClickAction::Confirm
    } else {
        DoubleClickAction::Edit
    }
}

#[cfg(test)]
mod ordinary_double_click_tests {
    use super::*;
    use crate::annotation::Point;

    const CANVAS: Rect = Rect {
        x: 0,
        y: 0,
        width: 400,
        height: 300,
    };
    const CROP: Rect = Rect {
        x: 80,
        y: 60,
        width: 200,
        height: 160,
    };

    fn editor() -> Editor {
        Editor::new(CANVAS, Some(CROP))
    }

    fn decision(editor: &Editor) -> DoubleClickAction {
        ordinary_double_click(editor, CANVAS, 2, true, false, false, false)
    }

    #[test]
    fn crop_and_object_edit_double_click_use_the_existing_confirm_action() {
        let mut editor = editor();
        assert_eq!(decision(&editor), DoubleClickAction::Confirm);
        editor.select_tool(Tool::Edit);
        assert_eq!(decision(&editor), DoubleClickAction::Confirm);
        assert_eq!(editor.crop, Some(CROP));
        assert!(!editor.document.can_undo());
    }

    #[test]
    fn single_triple_and_modified_clicks_keep_their_existing_editing_route() {
        let editor = editor();
        for count in [0, 1, 3, 4] {
            assert_eq!(
                ordinary_double_click(&editor, CANVAS, count, true, false, false, false),
                DoubleClickAction::None,
            );
        }
        assert_eq!(
            ordinary_double_click(&editor, CANVAS, 2, true, true, false, false),
            DoubleClickAction::None,
        );
    }

    #[test]
    fn images_pins_busy_hosts_and_active_input_cannot_confirm_on_double_click() {
        let editor = editor();
        for (screenshot, busy, input) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            assert_eq!(
                ordinary_double_click(&editor, CANVAS, 2, screenshot, false, busy, input),
                DoubleClickAction::None,
            );
        }
        assert!(!editor.document.can_undo());
    }

    #[test]
    fn missing_or_outdated_crop_never_confirms_or_exits_a_drawing_tool() {
        let mut editor = editor();
        editor.select_tool(Tool::Pen);
        assert_eq!(
            ordinary_double_click(&editor, Rect::full(100, 100), 2, true, false, false, false),
            DoubleClickAction::None,
        );
        editor.set_crop(None);
        assert_eq!(decision(&editor), DoubleClickAction::None);
    }

    #[test]
    fn drawing_and_selection_effect_tools_exit_to_edit_instead_of_output() {
        for tool in [
            Tool::Rectangle,
            Tool::Ellipse,
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
        ] {
            let mut editor = editor();
            editor.select_tool(tool);
            // 水印有独立输入草稿时仍交给原输入路线。
            if editor.text_draft.is_none() {
                assert_eq!(decision(&editor), DoubleClickAction::Edit, "{tool:?}");
            }
            assert_eq!(editor.tool, tool);
            assert_eq!(editor.crop, Some(CROP));
            assert!(!editor.document.can_undo());
        }
    }

    #[test]
    fn active_crop_and_object_gestures_are_not_committed_by_another_double_down() {
        let mut editor = editor();
        editor.pointer_down((100, 100), 4.);
        editor.pointer_move((140, 120));
        assert!(editor.is_dragging());
        assert_eq!(decision(&editor), DoubleClickAction::None);
        assert!(editor.cancel_gesture());
        assert_eq!(editor.crop, Some(CROP));
        assert!(!editor.document.can_undo());

        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((100, 90), 4.);
        editor.pointer_up((150, 130));
        let before = editor.document.items()[0].annotation.points.clone();
        let revision = editor.document.revision();
        editor.select_tool(Tool::Edit);
        editor.pointer_down((130, 110), 4.);
        editor.pointer_move((145, 125));
        assert!(editor.is_dragging());
        assert_eq!(decision(&editor), DoubleClickAction::None);
        assert!(editor.cancel_gesture());
        assert_eq!(editor.document.items()[0].annotation.points, before);
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.crop, Some(CROP));
    }

    #[test]
    fn arrow_pending_double_click_can_exit_without_a_ghost_or_losing_redo() {
        let mut editor = editor();
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((100, 90), 4.);
        editor.pointer_up((150, 130));
        editor.undo();
        assert!(editor.document.can_redo());
        editor.select_tool(Tool::Arrow);
        editor.pointer_down((120, 100), 4.);
        editor.pointer_up((120, 100));
        assert!(editor.has_arrow_draft());
        assert_eq!(decision(&editor), DoubleClickAction::Edit);
        editor.select_tool(Tool::Edit);
        assert!(!editor.has_pending_drawing());
        assert!(editor.document.items().is_empty());
        assert!(editor.document.can_redo());
        assert_eq!(editor.crop, Some(CROP));
    }

    #[test]
    fn polyline_double_click_keeps_its_draft_finish_route_and_one_command() {
        let mut editor = editor();
        editor.select_tool(Tool::Polyline);
        editor.pointer_down((100, 90), 4.);
        editor.pointer_up((100, 90));
        editor.pointer_down((160, 120), 4.);
        editor.pointer_up((160, 120));
        assert!(editor.has_polyline());
        assert_eq!(decision(&editor), DoubleClickAction::None);
        assert!(editor.insert_polyline_node((160, 120), 4.));
        assert_eq!(editor.tool, Tool::Polyline);
        assert!(!editor.has_polyline());
        assert_eq!(editor.document.items().len(), 1);
        editor.undo();
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
    }

    #[test]
    fn text_double_click_remains_in_the_text_input_route_without_committing() {
        let mut editor = editor();
        editor.select_tool(Tool::Text);
        editor.begin_text(None, Point { x: 100., y: 90. });
        editor.set_text("尚未提交".into());
        assert_eq!(decision(&editor), DoubleClickAction::None);
        assert_eq!(
            editor.text_draft.as_ref().unwrap().annotation.text,
            "尚未提交"
        );
        assert!(editor.document.items().is_empty());
        assert!(!editor.document.can_undo());
    }
}

#[cfg(test)]
mod mode_double_priority_tests {
    use super::*;
    use crate::{
        annotation::{Bounds, Point},
        crop_mask::Polygon,
        smart_mosaic::{RequestKey, SmartMosaic, SourceEpoch, TaskKind},
    };

    const CANVAS: Rect = Rect {
        x: 0,
        y: 0,
        width: 400,
        height: 300,
    };
    const CROP: Rect = Rect {
        x: 80,
        y: 60,
        width: 200,
        height: 160,
    };
    const INTERVAL: Duration = Duration::from_millis(500);
    const TOLERANCE: (i32, i32) = (4, 6);

    fn editor(tool: Tool) -> Editor {
        let mut editor = Editor::new(CANVAS, Some(CROP));
        editor.select_tool(Tool::Rectangle);
        editor.pointer_down((100, 90), 4.);
        editor.pointer_up((150, 130));
        editor.select_tool(tool);
        editor
    }

    fn decision(editor: &Editor, count: usize) -> DoubleClickAction {
        ordinary_double_click(editor, CANVAS, count, true, false, false, false)
    }

    fn up() -> CanvasClickUp {
        CanvasClickUp {
            screenshot: true,
            modified: false,
            busy: false,
            input_active: false,
            owned: true,
        }
    }

    fn first_down(
        sequence: &mut CanvasClickSequence,
        editor: &Editor,
        point: (i32, i32),
        at: Instant,
    ) {
        let count = sequence.count(None, 1, point, at);
        sequence.begin(
            count,
            ordinary_canvas_click_ready(editor, true, false, false, false),
            point,
            at,
            INTERVAL,
            TOLERANCE,
        );
    }

    fn smart_core(now: Instant, mode: u8) -> SmartMosaic {
        let mut core = SmartMosaic::default();
        core.activate(
            RequestKey {
                source: SourceEpoch(1),
                roi: CROP,
                backend: "WinRT".into(),
                profile: "default".into(),
                kind: TaskKind::Recognition,
            },
            now,
        );
        let request = core.begin_request(now).unwrap();
        if mode != 3 {
            assert!(core.complete_success(
                &request,
                if mode == 2 {
                    Vec::new()
                } else {
                    vec![Bounds {
                        x: 10.,
                        y: 10.,
                        width: 60.,
                        height: 30.,
                    }]
                },
            ));
        }
        core
    }

    #[test]
    fn smart_candidate_blank_empty_and_loading_clicks_finish_proof_then_exit_once() {
        let at = Instant::now();
        // 同一真实首击链：候选命中、候选外空白、成功空结果、识别尚未结束。
        for mode in 0..4 {
            let mut editor = editor(Tool::SmartMosaic);
            let base = editor.document.items().to_vec();
            let point = if mode == 1 { (200, 150) } else { (120, 90) };
            let mut core = smart_core(at, mode);
            assert!(core.is_active());
            let mut sequence = CanvasClickSequence::default();
            first_down(&mut sequence, &editor, point, at);
            let candidate_pointer = core.pointer_down(Point::from_tuple(point), 4.);
            assert_eq!(candidate_pointer, mode < 2);
            if candidate_pointer {
                if let Some(intent) =
                    core.pointer_up(Point::from_tuple(point), &editor.smart_applied_regions())
                {
                    let edit = editor
                        .prepare_smart_regions(intent.next_regions, Tool::Mosaic, editor.style)
                        .unwrap()
                        .unwrap();
                    assert!(editor.commit_smart_regions(edit));
                }
            }
            let pending = sequence.take_pending().unwrap();
            // 与真实 Smart Up 相同：清 pointer tracking 后以原 pending 完成首击。
            sequence.consumed();
            let mut seed = sequence.finish_ordinary(pending, &editor, CANVAS, point, up());
            assert!(seed.is_some());
            assert_eq!(
                editor.document.items().len(),
                base.len() + usize::from(mode == 0)
            );
            let items = editor.document.items().to_vec();
            let revision = editor.document.revision();
            let edit_revision = editor.document.edit_revision();
            let second = sequence.count(seed.take(), 2, point, at + Duration::from_millis(100));
            let action = decision(&editor, second);
            assert_eq!(action, DoubleClickAction::Edit);
            assert!(!action.starts_pointer());
            // 第二 Down 不进入候选 Toggle 或新对象手势。
            core.leave();
            sequence.consumed();
            editor.select_tool(Tool::Edit);
            sequence.after_double();
            assert!(!core.is_active());
            assert_eq!(editor.tool, Tool::Edit);
            assert_eq!(editor.crop, Some(CROP));
            assert_eq!(editor.document.items(), items);
            assert_eq!(editor.document.revision(), revision);
            assert_eq!(editor.document.edit_revision(), edit_revision);
            assert!(!editor.is_dragging());
            assert!(sequence.take_pending().is_none()); // double 的 Up 不再播种。
            for raw in [3, 4] {
                let count = sequence.count(None, raw, point, at + Duration::from_millis(200));
                assert_eq!(count, raw);
                assert_eq!(decision(&editor, count), DoubleClickAction::None);
                sequence.begin(count, true, point, at, INTERVAL, TOLERANCE);
                assert!(sequence.take_pending().is_none());
            }
            if mode == 0 {
                editor.undo();
                assert_eq!(editor.document.items(), base);
                editor.redo();
                assert_eq!(editor.document.items(), items);
            }
        }
    }

    #[test]
    fn smart_drag_return_rejected_up_cancel_and_failed_intent_never_publish_double_proof() {
        let at = Instant::now();
        let point = (120, 90);
        let mut editor = editor(Tool::SmartMosaic);
        let items = editor.document.items().to_vec();
        let revision = editor.document.revision();
        let mut core = smart_core(at, 0);
        let mut sequence = CanvasClickSequence::default();
        first_down(&mut sequence, &editor, point, at);
        assert!(core.pointer_down(Point::from_tuple(point), 4.));
        for position in [(160, 120), point] {
            sequence.moved(position, true);
            core.pointer_move(Point::from_tuple(position));
        }
        assert!(core
            .pointer_up(Point::from_tuple(point), &editor.smart_applied_regions())
            .is_none());
        let pending = sequence.take_pending().unwrap();
        assert!(sequence
            .finish_ordinary(pending, &editor, CANVAS, point, up())
            .is_none());
        let count = sequence.count(None, 2, point, at + Duration::from_millis(100));
        assert_eq!(count, 1);
        assert_eq!(decision(&editor, count), DoubleClickAction::None);

        for rejected in [
            CanvasClickUp { busy: true, ..up() },
            CanvasClickUp {
                modified: true,
                ..up()
            },
            CanvasClickUp {
                input_active: true,
                ..up()
            },
            CanvasClickUp {
                owned: false,
                ..up()
            },
        ] {
            first_down(&mut sequence, &editor, point, at);
            let pending = sequence.take_pending().unwrap();
            assert!(sequence
                .finish_ordinary(pending, &editor, CANVAS, point, rejected)
                .is_none());
            let count = sequence.count(None, 2, point, at);
            assert_eq!(decision(&editor, count), DoubleClickAction::None);
        }
        first_down(&mut sequence, &editor, point, at);
        sequence.modifiers_changed(true);
        sequence.modifiers_changed(false);
        let pending = sequence.take_pending().unwrap();
        assert!(sequence
            .finish_ordinary(pending, &editor, CANVAS, point, up())
            .is_none());
        for lost_focus in [false, true] {
            first_down(&mut sequence, &editor, point, at);
            let mut cancelled = smart_core(at, 0);
            assert!(cancelled.pointer_down(Point::from_tuple(point), 4.));
            if lost_focus {
                cancelled.leave();
            } else {
                cancelled.cancel_pointer();
            }
            assert_eq!(cancelled.is_active(), !lost_focus);
            assert!(cancelled
                .pointer_up(Point::from_tuple(point), &editor.smart_applied_regions())
                .is_none());
            sequence.consumed(); // cancel/失焦不保原 pending，随后 Up 不能完成首击。
            assert!(sequence.take_pending().is_none());
            assert_eq!(sequence.count(None, 2, point, at), 1);
        }
        first_down(&mut sequence, &editor, point, at);
        let mut invalid_style = editor.style;
        invalid_style.opacity = f32::NAN;
        assert!(editor
            .prepare_smart_regions(core.candidates().to_vec(), Tool::Mosaic, invalid_style)
            .is_err());
        let pending = sequence.take_pending().unwrap();
        assert!(sequence
            .finish_ordinary(
                pending,
                &editor,
                CANVAS,
                point,
                CanvasClickUp {
                    owned: false,
                    ..up()
                },
            )
            .is_none());
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.crop, Some(CROP));
        editor.undo();
        assert!(editor.document.items().is_empty());
        editor.redo();
        assert_eq!(editor.document.items(), items);
    }

    #[test]
    fn outside_eight_single_clicks_resize_but_proven_double_preempts_new_pointer() {
        let at = Instant::now();
        for tool in [Tool::Edit, Tool::Lasso, Tool::SmartMosaic] {
            for point in [
                (40, 30),
                (160, 30),
                (320, 30),
                (40, 140),
                (320, 140),
                (40, 260),
                (160, 260),
                (320, 260),
            ] {
                let mut editor = editor(tool);
                let items = editor.document.items().to_vec();
                let revision = editor.document.revision();
                let mut sequence = CanvasClickSequence::default();
                first_down(&mut sequence, &editor, point, at);
                assert!(decision(&editor, 1).starts_pointer());
                assert!(editor.begin_screenshot_crop_outside(point, 6.));
                assert!(editor.is_changing_crop());
                editor.pointer_up(point);
                let crop = editor.crop;
                assert_ne!(crop, Some(CROP));
                assert_eq!(editor.document.items(), items);
                let after_first_revision = editor.document.revision();
                let after_first_edit_revision = editor.document.edit_revision();
                assert!(after_first_revision >= revision);
                let pending = sequence.take_pending().unwrap();
                let seed = sequence.finish_ordinary(pending, &editor, CANVAS, point, up());
                assert!(seed.is_some());
                let second = sequence.count(seed, 2, point, at + Duration::from_millis(100));
                let action = decision(&editor, second);
                assert_eq!(
                    action,
                    if tool == Tool::Edit {
                        DoubleClickAction::Confirm
                    } else {
                        DoubleClickAction::Edit
                    },
                );
                assert!(!action.starts_pointer());
                // 相同生产优先级：有效 double 不再执行 outside Resize 或 Smart Down。
                if action.starts_pointer() {
                    editor.begin_screenshot_crop_outside(point, 6.);
                }
                assert!(!editor.is_dragging());
                if action == DoubleClickAction::Edit {
                    editor.select_tool(Tool::Edit);
                }
                assert_eq!(editor.crop, crop);
                assert_eq!(editor.document.items(), items);
                assert_eq!(editor.document.revision(), after_first_revision);
                assert_eq!(editor.document.edit_revision(), after_first_edit_revision);
            }
        }
    }

    #[test]
    fn lasso_closed_draft_boundary_double_exits_without_applying_or_changing_document_history() {
        let at = Instant::now();
        let mut editor = editor(Tool::Lasso);
        let applied = Polygon::new(&[
            Point { x: 100., y: 90. },
            Point { x: 160., y: 90. },
            Point { x: 160., y: 140. },
            Point { x: 100., y: 140. },
        ])
        .unwrap();
        editor.document.set_crop_mask(Some(applied.clone()));
        editor.lasso_draft = Polygon::new(&[
            Point { x: 110., y: 100. },
            Point { x: 200., y: 100. },
            Point { x: 200., y: 160. },
            Point { x: 110., y: 160. },
        ]);
        let draft = editor.lasso_draft.clone();
        let items = editor.document.items().to_vec();
        let revision = editor.document.revision();
        let edit_revision = editor.document.edit_revision();
        let mut sequence = CanvasClickSequence::default();
        let point = (280, 140); // 右边界在半开 Rect 外，原 outside 入口仍会命中。
        first_down(&mut sequence, &editor, point, at);
        assert!(editor.begin_screenshot_crop_outside(point, 6.));
        editor.pointer_up(point);
        assert_eq!(editor.crop, Some(CROP));
        assert_eq!(editor.lasso_draft, draft);
        assert_eq!(editor.document.crop_mask, Some(applied.clone()));
        let pending = sequence.take_pending().unwrap();
        let seed = sequence.finish_ordinary(pending, &editor, CANVAS, point, up());
        let count = sequence.count(seed, 2, point, at + Duration::from_millis(100));
        let action = decision(&editor, count);
        assert_eq!(action, DoubleClickAction::Edit);
        assert!(!action.starts_pointer());
        editor.select_tool(Tool::Edit);
        assert!(editor.lasso_draft.is_none()); // 离开 Lasso 原语义：丢未应用草稿。
        assert_eq!(editor.document.crop_mask, Some(applied));
        assert_eq!(editor.crop, Some(CROP));
        assert_eq!(editor.document.items(), items);
        assert_eq!(editor.document.revision(), revision);
        assert_eq!(editor.document.edit_revision(), edit_revision);
        editor.undo(); // double 没写历史；撤销的仍是原应用 mask。
        assert!(editor.document.crop_mask.is_none());
        assert_eq!(editor.document.items(), items);
    }
}

#[cfg(test)]
mod text_blank_double_tests {
    use super::*;
    use crate::annotation::{Annotation, Style};

    const CANVAS: Rect = Rect {
        x: 0,
        y: 0,
        width: 700,
        height: 500,
    };
    const CROP: Rect = Rect {
        x: 20,
        y: 20,
        width: 600,
        height: 400,
    };
    const BLANK: Point = Point { x: 520., y: 360. };

    fn host() -> CanvasClickUp {
        CanvasClickUp {
            screenshot: true,
            modified: false,
            busy: false,
            input_active: false,
            owned: true,
        }
    }
    fn text(point: Point, value: &str) -> Annotation {
        let mut annotation = Annotation::new(Tool::Text, point, Style::default());
        annotation.update(Point {
            x: point.x + 100.,
            y: point.y + 40.,
        });
        annotation.text = value.into();
        annotation
    }
    fn draft(existing: bool) -> Editor {
        let mut editor = Editor::new(CANVAS, Some(CROP));
        let mut rectangle =
            Annotation::new(Tool::Rectangle, Point { x: 50., y: 220. }, Style::default());
        rectangle.update(Point { x: 160., y: 290. });
        editor.document.insert(rectangle);
        let point = Point { x: 60., y: 50. };
        let id = existing.then(|| editor.document.insert(text(point, "原文字")));
        editor.select_tool(Tool::Text);
        editor.begin_text(id, point);
        editor.set_text("完成的文字".into());
        editor
    }
    fn begin(sequence: &mut CanvasClickSequence, editor: &Editor, point: Point, at: Instant) {
        let count = sequence.count(None, 1, point.rounded(), at);
        let ready = ordinary_canvas_click_ready(editor, true, false, false, false);
        sequence.begin(
            count,
            ready,
            point.rounded(),
            at,
            Duration::from_millis(500),
            (4, 4),
        );
    }

    #[test]
    fn text_blank_first_up_then_double_exits_edit_with_one_text_command_and_unchanged_crop() {
        for route in 0..3 {
            let mut editor = draft(route != 0);
            if route == 2 {
                editor.set_text("   ".into());
            }
            let before = editor.document.items().to_vec();
            let before_revision = editor.document.revision();
            let before_edit_revision = editor.document.edit_revision();
            let mut sequence = CanvasClickSequence::default();
            let at = Instant::now();
            assert!(finish_ordinary_text_blank_down(
                &mut editor,
                BLANK,
                6.,
                CANVAS,
                host()
            ));
            assert!(editor.text_draft.is_none());
            assert_eq!(editor.tool, Tool::Text);
            assert_eq!(editor.crop, Some(CROP));
            assert!(ordinary_canvas_click_ready(
                &editor, true, false, false, false
            ));
            let completed = editor.document.items().to_vec();
            let revision = editor.document.revision();
            let edit_revision = editor.document.edit_revision();
            assert_ne!(completed, before);
            // 新建、修改和把已有文字改空后删除，各只提交一个实际文档命令。
            assert_eq!(revision, before_revision + 1);
            assert_eq!(edit_revision, before_edit_revision + 1);
            assert_eq!(
                completed.len(),
                if route == 0 {
                    before.len() + 1
                } else if route == 2 {
                    before.len() - 1
                } else {
                    before.len()
                }
            );
            begin(&mut sequence, &editor, BLANK, at);
            // 结束编辑的 Down 不走 Editor::pointer_down，新框直到本次真实 Up 都不存在。
            assert!(!editor.is_dragging());
            assert!(editor.text_draft.is_none());
            let mut missing_up = CanvasClickSequence::default();
            begin(&mut missing_up, &editor, BLANK, at);
            let raw_double =
                missing_up.count(None, 2, BLANK.rounded(), at + Duration::from_millis(50));
            assert_eq!(
                ordinary_double_click(&editor, CANVAS, raw_double, true, false, false, false),
                DoubleClickAction::None
            );
            let pending = sequence.take_pending().unwrap();
            let seed = sequence.finish_ordinary(pending, &editor, CANVAS, BLANK.rounded(), host());
            assert!(seed.is_some());
            let count = sequence.count(seed, 2, BLANK.rounded(), at + Duration::from_millis(50));
            let action = ordinary_double_click(&editor, CANVAS, count, true, false, false, false);
            assert_eq!(action, DoubleClickAction::Edit);
            assert!(!action.starts_pointer());
            editor.select_tool(Tool::Edit);
            sequence.after_double();
            assert_eq!(editor.tool, Tool::Edit);
            assert_eq!(editor.document.items(), completed);
            assert_eq!(editor.document.revision(), revision);
            assert_eq!(editor.document.edit_revision(), edit_revision);
            assert_eq!(editor.crop, Some(CROP));
            assert!(editor.text_draft.is_none());
            for raw in [3, 4] {
                let count =
                    sequence.count(None, raw, BLANK.rounded(), at + Duration::from_millis(80));
                assert_eq!(
                    ordinary_double_click(&editor, CANVAS, count, true, false, false, false),
                    DoubleClickAction::None
                );
            }
            editor.undo();
            assert_eq!(editor.document.items(), before);
            editor.redo();
            assert_eq!(editor.document.items(), completed);
            assert_eq!(editor.crop, Some(CROP));
        }
    }

    #[test]
    fn text_blank_admission_keeps_text_handles_other_targets_crop_boundaries_and_original_creation_routes(
    ) {
        let editor = draft(false);
        let annotation = editor.text_draft.as_ref().unwrap().annotation.clone();
        let bounds = annotation.local_bounds();
        for point in [
            Point {
                x: bounds.x + 8.,
                y: bounds.y + 8.,
            },
            Point {
                x: bounds.x - 3.,
                y: bounds.y,
            },
            Point { x: 680., y: 480. },
        ] {
            let mut editor = draft(false);
            let before = editor.text_draft.as_ref().unwrap().annotation.clone();
            let revision = editor.document.revision();
            assert!(!finish_ordinary_text_blank_down(
                &mut editor,
                point,
                6.,
                CANVAS,
                host()
            ));
            assert_eq!(editor.text_draft.as_ref().unwrap().annotation, before);
            assert_eq!(editor.document.revision(), revision);
        }
        let mut editor = draft(false);
        let other = editor
            .document
            .insert(text(Point { x: 420., y: 220. }, "另一段文字"));
        let point = Point { x: 440., y: 235. };
        let before = editor.text_draft.as_ref().unwrap().annotation.clone();
        for target in [point, Point { x: 417., y: 220. }] {
            assert!(!finish_ordinary_text_blank_down(
                &mut editor,
                target,
                6.,
                CANVAS,
                host()
            ));
            assert_eq!(editor.text_draft.as_ref().unwrap().annotation, before);
        }
        // 原先点击另一文字对象的真实 Editor 路线继续命中，不被当作 blank 消费。
        editor.pointer_down_at(point, 6., false);
        assert_eq!(editor.selected, Some(other));
        editor.cancel_gesture();
        for rejected in [
            CanvasClickUp {
                screenshot: false,
                ..host()
            },
            CanvasClickUp {
                modified: true,
                ..host()
            },
            CanvasClickUp {
                busy: true,
                ..host()
            },
            CanvasClickUp {
                input_active: true,
                ..host()
            },
            CanvasClickUp {
                owned: false,
                ..host()
            },
        ] {
            let mut editor = draft(false);
            let before = editor.text_draft.as_ref().unwrap().annotation.clone();
            assert!(!finish_ordinary_text_blank_down(
                &mut editor,
                BLANK,
                6.,
                CANVAS,
                rejected
            ));
            assert_eq!(editor.text_draft.as_ref().unwrap().annotation, before);
        }
        let mut editor = draft(false);
        editor.crop = None;
        assert!(!finish_ordinary_text_blank_down(
            &mut editor,
            BLANK,
            6.,
            CANVAS,
            host()
        ));
        let mut editor = Editor::new(CANVAS, Some(CROP));
        editor.select_tool(Tool::Text);
        assert!(!finish_ordinary_text_blank_down(
            &mut editor,
            BLANK,
            6.,
            CANVAS,
            host()
        ));
        editor.pointer_down_at(BLANK, 6., false);
        assert!(editor.text_draft.is_some());
        let mut editor = draft(false);
        assert!(!finish_ordinary_text_blank_down(
            &mut editor,
            BLANK,
            6.,
            CANVAS,
            CanvasClickUp {
                screenshot: false,
                ..host()
            }
        ));
        editor.pointer_down_at(BLANK, 6., false);
        assert!(editor.text_draft.is_some());
        assert_ne!(editor.text_draft.as_ref().unwrap().annotation, annotation);
        let mut editor = Editor::new(CANVAS, Some(CROP));
        editor.select_tool(Tool::Polyline);
        editor.pointer_down((90, 100), 6.);
        editor.pointer_up((90, 100));
        assert!(editor.has_polyline());
        assert!(!finish_ordinary_text_blank_down(
            &mut editor,
            BLANK,
            6.,
            CANVAS,
            host()
        ));
        assert!(editor.has_polyline());
    }

    #[test]
    fn text_blank_drag_return_cancel_modified_owner_loss_and_failed_admission_never_publish_double_proof(
    ) {
        for route in 0..6 {
            let mut editor = draft(false);
            let mut sequence = CanvasClickSequence::default();
            let at = Instant::now();
            assert!(finish_ordinary_text_blank_down(
                &mut editor,
                BLANK,
                6.,
                CANVAS,
                host()
            ));
            let completed = editor.document.items().to_vec();
            let revision = editor.document.revision();
            begin(&mut sequence, &editor, BLANK, at);
            let mut up = host();
            match route {
                0 => {
                    sequence.moved((560, 360), true);
                    sequence.moved(BLANK.rounded(), true);
                }
                1 => sequence.consumed(), // 取消/失焦都由同一生产清理方法撤 pending。
                2 => {
                    sequence.modifiers_changed(true);
                    sequence.modifiers_changed(false);
                }
                3 => up.owned = false, // 捕获失败或 Up 不属于本次 Down。
                4 => up.input_active = true,
                5 => up.busy = true,
                _ => unreachable!(),
            }
            let seed = sequence.take_pending().and_then(|pending| {
                sequence.finish_ordinary(pending, &editor, CANVAS, BLANK.rounded(), up)
            });
            assert!(seed.is_none(), "route={route}");
            let count = sequence.count(seed, 2, BLANK.rounded(), at + Duration::from_millis(50));
            assert_eq!(count, 1);
            assert_eq!(
                ordinary_double_click(&editor, CANVAS, count, true, false, false, false),
                DoubleClickAction::None
            );
            assert_eq!(editor.document.items(), completed);
            assert_eq!(editor.document.revision(), revision);
            assert_eq!(editor.crop, Some(CROP));
            assert!(editor.text_draft.is_none());
        }
        let mut editor = draft(false);
        editor
            .text_draft
            .as_mut()
            .unwrap()
            .annotation
            .points
            .clear();
        let before = editor.document.items().to_vec();
        assert!(!finish_ordinary_text_blank_down(
            &mut editor,
            BLANK,
            6.,
            CANVAS,
            host()
        ));
        assert!(editor.text_draft.is_some());
        assert_eq!(editor.document.items(), before);
        let mut sequence = CanvasClickSequence::default();
        sequence.begin(
            1,
            ordinary_canvas_click_ready(&editor, true, false, false, false),
            BLANK.rounded(),
            Instant::now(),
            Duration::from_millis(500),
            (4, 4),
        );
        assert!(sequence.take_pending().is_none());
    }
}

#[cfg(test)]
mod click_seed_tests {
    use super::*;

    #[test]
    fn completed_native_click_upgrades_only_the_matching_next_single_down() {
        let down_at = Instant::now();
        let seed = ClickSeed::new((120, 90), down_at, Duration::from_millis(500), (4, 6));
        assert_eq!(
            seed.click_count(1, (120, 90), down_at + Duration::from_millis(200)),
            2
        );
        assert_eq!(seed.click_count(1, (120, 90), down_at), 2);
    }

    #[test]
    fn system_physical_rectangle_includes_both_axis_edges_and_diagonal() {
        let down_at = Instant::now();
        let seed = ClickSeed::new((-100, 30), down_at, Duration::from_millis(500), (4, 6));
        for point in [(-104, 24), (-96, 36), (-104, 36), (-96, 24)] {
            assert_eq!(seed.click_count(1, point, down_at), 2);
        }
        for point in [(-105, 30), (-95, 30), (-100, 23), (-100, 37)] {
            assert_eq!(seed.click_count(1, point, down_at), 1);
        }
    }

    #[test]
    fn double_click_interval_is_strict_and_does_not_extend_during_replay() {
        let down_at = Instant::now();
        let interval = Duration::from_millis(500);
        let seed = ClickSeed::new((50, 60), down_at, interval, (4, 4));
        assert_eq!(
            seed.click_count(1, (50, 60), down_at + interval - Duration::from_nanos(1)),
            2
        );
        assert_eq!(seed.click_count(1, (50, 60), down_at + interval), 1);
        assert_eq!(
            seed.click_count(1, (50, 60), down_at + Duration::from_secs(2)),
            1
        );
    }

    #[test]
    fn receiving_time_before_seed_cannot_upgrade_or_panic() {
        let before = Instant::now();
        let down_at = before + Duration::from_millis(1);
        let seed = ClickSeed::new((0, 0), down_at, Duration::from_millis(500), (4, 4));
        assert_eq!(seed.click_count(1, (0, 0), before), 1);
    }

    #[test]
    fn existing_backend_double_and_triple_counts_are_not_rewritten() {
        let down_at = Instant::now();
        let seed = ClickSeed::new((50, 60), down_at, Duration::from_millis(500), (4, 4));
        for count in [0, 2, 3, 4] {
            assert_eq!(seed.click_count(count, (50, 60), down_at), count);
        }
    }

    #[test]
    fn extreme_physical_coordinates_do_not_overflow_the_distance_check() {
        let down_at = Instant::now();
        let seed = ClickSeed::new(
            (i32::MIN, i32::MAX),
            down_at,
            Duration::from_millis(500),
            (4, 6),
        );
        assert_eq!(seed.click_count(1, (i32::MAX, i32::MIN), down_at), 1);
        assert_eq!(
            seed.click_count(1, (i32::MIN + 4, i32::MAX - 6), down_at),
            2
        );
    }

    #[test]
    fn zero_interval_or_invalid_system_tolerance_cannot_upgrade() {
        let down_at = Instant::now();
        assert_eq!(
            ClickSeed::new((1, 2), down_at, Duration::ZERO, (4, 4)).click_count(1, (1, 2), down_at),
            1
        );
        assert_eq!(
            ClickSeed::new((1, 2), down_at, Duration::from_millis(500), (-1, 4)).click_count(
                1,
                (1, 2),
                down_at
            ),
            1
        );
        assert_eq!(
            ClickSeed::new((1, 2), down_at, Duration::from_millis(500), (4, -1)).click_count(
                1,
                (1, 2),
                down_at
            ),
            1
        );
    }
}
