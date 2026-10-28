//! 贴图专属 OCR 请求槽；取消权独立于工作令牌，旧回执不能释放新 Owner。
use crate::{
    recognition::task::{is_cancelled, CancelOwner, TaskToken},
    Result,
};
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct Request<S: Copy + PartialEq> {
    identity: Arc<()>,
    pub(super) source: S,
    pub(super) generation: u64,
}

impl<S: Copy + PartialEq> Request<S> {
    /// 只借用真实任务身份供临时反馈；不授予取消或发布结果资格。
    pub(super) fn identity(&self) -> &Arc<()> {
        &self.identity
    }
}

struct Running<S: Copy + PartialEq> {
    request: Request<S>,
    owner: CancelOwner,
}

/// 只持有当前请求的 Owner；析构由 CancelOwner 兜底取消自己持有的请求。
pub(super) struct TaskSlot<S: Copy + PartialEq> {
    active: Option<Running<S>>,
}

impl<S: Copy + PartialEq> Default for TaskSlot<S> {
    fn default() -> Self {
        Self { active: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Receipt {
    Current,
    SourceChanged,
    Stale,
}

impl<S: Copy + PartialEq> TaskSlot<S> {
    /// 宿主须先显式取消旧请求；不能借替换 Option 隐藏取消控制错误。
    pub(super) fn begin(&mut self, source: S, generation: u64) -> (Request<S>, TaskToken) {
        assert!(self.active.is_none(), "开始 OCR 前须先取消现有请求");
        let owner = CancelOwner::new();
        let token = owner.token();
        let request = Request {
            identity: Arc::new(()),
            source,
            generation,
        };
        self.active = Some(Running {
            request: request.clone(),
            owner,
        });
        (request, token)
    }

    /// 先释放回执资格，再控制本任务；返回成功不代表后端 Run 已退出。
    pub(super) fn cancel(&mut self) -> Result<bool> {
        let Some(running) = self.active.take() else {
            return Ok(false);
        };
        let result = running.owner.cancel();
        drop(running);
        result
    }

    pub(super) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn source_matches(&self, current_source: S, generation: u64) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.request.source == current_source && active.request.generation == generation
        })
    }

    /// 仅读取仍属于当前来源/内容代次的活动请求，不复制 Owner。
    pub(super) fn active_request(&self, source: S, generation: u64) -> Option<&Request<S>> {
        self.active
            .as_ref()
            .filter(|active| {
                active.request.source == source
                    && active.request.generation == generation
                    && !active.owner.token().is_cancelled()
            })
            .map(|active| &active.request)
    }

    /// 工作端先完成 Outcome 分类；本方法资格判断后才释放 Owner，不读取其令牌。
    pub(super) fn finish(
        &mut self,
        request: &Request<S>,
        current_source: S,
        generation: u64,
    ) -> Receipt {
        let Some(active) = self.active.as_ref() else {
            return Receipt::Stale;
        };
        if !Arc::ptr_eq(&active.request.identity, &request.identity) {
            return Receipt::Stale;
        }
        let receipt = if active.request.source == request.source
            && active.request.source == current_source
            && active.request.generation == request.generation
            && active.request.generation == generation
        {
            Receipt::Current
        } else {
            Receipt::SourceChanged
        };
        drop(self.active.take());
        receipt
    }
}

/// OCR 可取消后继续关闭／保存推进；真正图像处理仍沿用现有阻挡合同。
pub(super) fn blocks_close(processing: bool, has_ocr: bool) -> bool {
    processing && !has_ocr
}

pub(super) enum Outcome<T> {
    Completed(T),
    Cancelled,
    Failed(String),
}

impl<T> Outcome<T> {
    /// 先识别专用取消类型，再把其它真实错误转换成展示文本。
    pub(super) fn from_result(result: Result<T>) -> Self {
        match result {
            Ok(value) => Self::Completed(value),
            Err(error) if is_cancelled(error.as_ref()) => Self::Cancelled,
            Err(error) => Self::Failed(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        geometry::Rect,
        recognition::{
            layout::CopyLayoutMode,
            task::{OcrCancelled, RunCancellation},
            HorizontalDirection, RecognizedLine, RecognizedText,
        },
    };
    use std::{
        error::Error,
        fmt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    fn recognized_text() -> RecognizedText {
        let rows = ["English first line", "continues here"];
        RecognizedText {
            text: rows.join("\n"),
            regions: Vec::new(),
            words: Vec::new(),
            lines: rows
                .iter()
                .enumerate()
                .map(|(index, text)| RecognizedLine {
                    graphemes: None,
                    text: (*text).into(),
                    rect: Some(Rect {
                        x: 20,
                        y: 20 + index as u32 * 26,
                        width: 200,
                        height: 20,
                    }),
                    horizontal_direction: HorizontalDirection::LeftToRight,
                    geometry: None,
                })
                .collect(),
        }
    }

    #[test]
    fn pin_ocr_task_normal_finish_preserves_completed_text_and_layout_after_owner_drop() {
        let mut slot = TaskSlot::<u64>::default();
        let (request, token) = slot.begin(41, 7);
        let text = recognized_text();
        let outcome = Outcome::from_result(Ok((text.text.clone(), text.copy_layout())));
        drop(text);
        assert!(!token.is_cancelled(), "完成回执前 Owner 仍持有任务");

        assert_eq!(slot.finish(&request, 41, 7), Receipt::Current);

        assert!(!slot.is_active());
        assert!(token.is_cancelled(), "Owner 正常释放后兜底标记可以变为取消");
        let Outcome::Completed((original, layout)) = outcome else {
            panic!("先分类的已完成结果不能被 Owner 析构追认成取消");
        };
        assert_eq!(original, "English first line\ncontinues here");
        assert_eq!(layout.text().as_bytes(), original.as_bytes());
        assert_eq!(
            layout
                .copy(0..original.len(), CopyLayoutMode::Auto)
                .unwrap(),
            "English first line continues here"
        );
        assert_eq!(slot.finish(&request.clone(), 41, 7), Receipt::Stale);
    }

    #[test]
    fn pin_ocr_task_cancel_replace_rejects_old_receipt_even_with_reused_numbers() {
        let mut slot = TaskSlot::<u64>::default();
        let (old, old_token) = slot.begin(41, u64::MAX);
        assert!(slot.cancel().unwrap());
        assert!(old_token.is_cancelled());
        let (current, current_token) = slot.begin(41, u64::MAX);

        assert_eq!(slot.finish(&old.clone(), 41, u64::MAX), Receipt::Stale);
        assert!(slot.is_active(), "旧回执不得取走新请求 Owner");
        assert!(slot.source_matches(41, u64::MAX));
        current_token.checkpoint().unwrap();
        assert_eq!(slot.finish(&current, 41, u64::MAX), Receipt::Current);
        assert!(current_token.is_cancelled());
    }

    #[test]
    fn pin_ocr_task_changed_source_or_generation_releases_only_matching_request() {
        for (source, generation) in [(42, 7), (41, 8)] {
            let mut slot = TaskSlot::<u64>::default();
            let (request, token) = slot.begin(41, 7);
            assert!(!slot.source_matches(source, generation));
            assert_eq!(
                slot.finish(&request, source, generation),
                Receipt::SourceChanged
            );
            assert!(!slot.is_active());
            assert!(token.is_cancelled());
            assert_eq!(slot.finish(&request, source, generation), Receipt::Stale);
        }
    }

    #[test]
    fn pin_ocr_task_cross_pin_receipt_cannot_release_another_owner() {
        let mut a = TaskSlot::<u64>::default();
        let mut b = TaskSlot::<u64>::default();
        let (request_a, token_a) = a.begin(41, 7);
        let (request_b, token_b) = b.begin(41, 7);

        assert_eq!(a.finish(&request_b, 41, 7), Receipt::Stale);
        assert_eq!(b.finish(&request_a, 41, 7), Receipt::Stale);
        assert!(a.is_active() && b.is_active());
        token_a.checkpoint().unwrap();
        token_b.checkpoint().unwrap();
        assert_eq!(a.finish(&request_a, 41, 7), Receipt::Current);
        token_b.checkpoint().unwrap();
    }

    #[derive(Debug)]
    struct ControlFailure;

    impl fmt::Display for ControlFailure {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("终止控制真实失败")
        }
    }

    impl Error for ControlFailure {}

    #[derive(Default)]
    struct FailingRun(AtomicUsize);

    impl RunCancellation for FailingRun {
        fn terminate(&self) -> Result<()> {
            self.0.fetch_add(1, Ordering::AcqRel);
            Err(ControlFailure.into())
        }
    }

    #[test]
    fn pin_ocr_task_cancel_control_error_releases_slot_and_keeps_original_error() {
        let mut slot = TaskSlot::<u64>::default();
        let (old, old_token) = slot.begin(41, 7);
        let run = Arc::new(FailingRun::default());
        let registration = old_token.register(run.clone()).unwrap();

        let error = slot.cancel().expect_err("真实终止控制失败必须显式上报");

        assert!(error.downcast_ref::<ControlFailure>().is_some());
        assert!(!is_cancelled(error.as_ref()));
        assert!(!slot.is_active(), "控制失败也不能恢复已释放的回执资格");
        assert!(old_token.is_cancelled());
        assert_eq!(run.0.load(Ordering::Acquire), 1);
        let (current, current_token) = slot.begin(41, 7);
        assert_eq!(slot.finish(&old, 41, 7), Receipt::Stale);
        current_token.checkpoint().unwrap();
        drop(registration);
        assert_eq!(slot.finish(&current, 41, 7), Receipt::Current);
    }

    #[test]
    fn pin_ocr_task_empty_and_repeated_cancel_do_not_touch_unrelated_token() {
        let unrelated = CancelOwner::new();
        let unrelated_token = unrelated.token();
        let mut slot = TaskSlot::<u64>::default();
        assert!(!slot.is_active());
        assert!(!slot.source_matches(41, 7));
        assert!(!slot.cancel().unwrap());
        let (_, token) = slot.begin(41, 7);
        assert!(slot.cancel().unwrap());
        assert!(!slot.cancel().unwrap());
        assert!(token.is_cancelled());
        unrelated_token.checkpoint().unwrap();
    }

    #[test]
    fn pin_ocr_task_slot_drop_cancels_only_its_owner() {
        let mut a = TaskSlot::<u64>::default();
        let mut b = TaskSlot::<u64>::default();
        let (_, token_a) = a.begin(41, 7);
        let (_, token_b) = b.begin(41, 7);
        drop(a);
        assert!(token_a.is_cancelled());
        assert!(b.is_active());
        token_b.checkpoint().unwrap();
    }

    #[test]
    fn pin_ocr_task_begin_refuses_silent_owner_replacement() {
        let mut slot = TaskSlot::<u64>::default();
        let (request, token) = slot.begin(41, 7);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            slot.begin(42, 8);
        }));
        assert!(result.is_err(), "宿主遗漏显式取消必须暴露");
        assert!(slot.source_matches(41, 7));
        token.checkpoint().unwrap();
        assert_eq!(slot.finish(&request, 41, 7), Receipt::Current);
    }

    #[test]
    fn pin_ocr_task_outcome_distinguishes_typed_cancel_from_real_error_text() {
        assert!(matches!(
            Outcome::<u64>::from_result(Err(OcrCancelled.into())),
            Outcome::Cancelled
        ));
        match Outcome::<u64>::from_result(Err("OCR 任务已取消".into())) {
            Outcome::Failed(message) => assert_eq!(message, "OCR 任务已取消"),
            _ => panic!("普通错误即使文案相同也不能冒充专用取消类型"),
        }
        match Outcome::<u64>::from_result(Err(ControlFailure.into())) {
            Outcome::Failed(message) => assert_eq!(message, "终止控制真实失败"),
            _ => panic!("真实控制错误必须保持失败分类"),
        }
        assert!(matches!(
            Outcome::from_result(Ok(123)),
            Outcome::Completed(123)
        ));
    }

    #[test]
    fn pin_ocr_task_policy_save_refusal_or_close_failure_keeps_owner_active() {
        // 仅覆盖宿主策略：未真正关闭时不调用 cancel；不构成 GUI 入口验证。
        for reason in ["正在保存拒绝关闭", "保存失败仍保留窗口"] {
            let mut slot = TaskSlot::<u64>::default();
            let (request, token) = slot.begin(41, 7);
            assert!(slot.is_active(), "{reason} 时须保留 Owner");
            assert!(slot.source_matches(41, 7));
            token.checkpoint().unwrap();
            assert_eq!(slot.finish(&request, 41, 7), Receipt::Current);
        }
    }

    #[test]
    fn pin_ocr_task_policy_true_close_cancels_a_without_cancelling_b() {
        // 仅覆盖真关闭显式 cancel 的策略；真实 PinView 关闭入口另行验证。
        let mut a = TaskSlot::<u64>::default();
        let mut b = TaskSlot::<u64>::default();
        let (request_a, token_a) = a.begin(41, 7);
        let (request_b, token_b) = b.begin(41, 7);
        assert!(a.cancel().unwrap());
        assert!(!a.is_active());
        assert!(token_a.is_cancelled());
        assert_eq!(a.finish(&request_a, 41, 7), Receipt::Stale);
        assert!(b.is_active());
        token_b.checkpoint().unwrap();
        assert_eq!(b.finish(&request_b, 41, 7), Receipt::Current);
    }

    #[test]
    fn pin_ocr_task_blocks_close_only_for_non_ocr_image_processing() {
        assert!(blocks_close(true, false), "真实图像处理仍阻挡推进");
        assert!(!blocks_close(true, true), "OCR 取消不能阻挡真正保存推进");
        assert!(!blocks_close(false, false));
        assert!(!blocks_close(false, true));
    }
}
