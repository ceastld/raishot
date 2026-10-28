use super::*;
use crate::recognition::task::{OcrCancelled, RunCancellation};
use std::{
    error::Error,
    fmt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

#[derive(Default)]
struct CountingRun {
    calls: AtomicUsize,
}

impl CountingRun {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Acquire)
    }
}

impl RunCancellation for CountingRun {
    fn terminate(&self) -> crate::Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[derive(Debug)]
struct RealFailure;

impl fmt::Display for RealFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("模型输出真实失败")
    }
}

impl Error for RealFailure {}

#[derive(Default)]
struct FailingRun {
    calls: AtomicUsize,
}

impl RunCancellation for FailingRun {
    fn terminate(&self) -> crate::Result<()> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        Err(RealFailure.into())
    }
}

#[derive(Clone, PartialEq, Eq)]
struct KeyWithoutDefault(u32);

#[test]
fn empty_slot_has_no_identity_key_or_worker_token() {
    let slot = TaskSlot::<KeyWithoutDefault>::default();
    assert!(slot.current_id().is_none());
    assert!(slot.key().is_none());
    assert!(slot.token().is_none());
    assert!(!slot.owns(0));
}

#[test]
fn start_publishes_matching_identity_key_and_live_worker_token() {
    let mut slot = TaskSlot::default();
    let request = slot.start(KeyWithoutDefault(7));
    assert_ne!(request.id, 0);
    assert_eq!(request.key.0, 7);
    assert_eq!(slot.key().map(|key| key.0), Some(7));
    assert_eq!(slot.current_id(), Some(request.id));
    assert!(slot.owns(request.id));
    request.token.checkpoint().expect("新请求可执行");
    slot.token().expect("在途请求有令牌").checkpoint().unwrap();
}

#[test]
fn replacing_request_cancels_only_previous_registered_run() {
    let mut slot = TaskSlot::default();
    let old = slot.start("旧来源");
    let run = Arc::new(CountingRun::default());
    let registration = old.token.register(run.clone()).unwrap();
    let current = slot.start("新来源");
    assert!(old.token.is_cancelled());
    assert!(!current.token.is_cancelled());
    assert_eq!(run.calls(), 1);
    assert!(slot.owns(current.id));
    assert!(!slot.owns(old.id));
    drop(registration);
}

#[test]
fn repeated_same_key_still_gets_distinct_identity() {
    let mut slot = TaskSlot::default();
    let first = slot.start("同一源与范围");
    let second = slot.start("同一源与范围");
    assert_ne!(first.id, second.id);
    assert!(first.token.is_cancelled());
    assert!(!second.token.is_cancelled());
    assert!(!slot.finish(first.id));
    assert!(slot.owns(second.id));
}

#[test]
fn old_completed_receipt_cannot_release_new_processing_gate() {
    let mut slot = TaskSlot::default();
    let old = slot.start("A");
    let current = slot.start("B");
    let mut processing = true;
    let old_receipt = Receipt::from_result(Ok("A 的文字"));
    if slot.finish_receipt(old.id, old_receipt).is_some() {
        processing = false;
    }
    assert!(processing, "仅匹配回执才可清宿主的 processing");
    assert_eq!(slot.current_id(), Some(current.id));
    assert_eq!(slot.key(), Some(&"B"));
    assert!(!current.token.is_cancelled());
}

#[test]
fn old_error_and_cancelled_receipts_leave_new_owner_untouched() {
    let mut slot = TaskSlot::default();
    let old = slot.start("A");
    let current = slot.start("B");
    assert!(slot
        .finish_receipt::<()>(old.id, Receipt::Failed("旧错误".into()))
        .is_none());
    assert!(slot
        .finish_receipt::<()>(old.id, Receipt::Cancelled)
        .is_none());
    assert!(slot.owns(current.id));
    current.token.checkpoint().expect("旧回执不得取消 B");
}

#[test]
fn classified_success_survives_normal_owner_release() {
    let mut slot = TaskSlot::default();
    let request = slot.start("A");
    let receipt = Receipt::from_result(Ok("完成文字"));
    let received = slot.finish_receipt(request.id, receipt);
    assert!(request.token.is_cancelled(), "Owner 析构确实会置取消位");
    assert_eq!(received, Some(Receipt::Completed("完成文字")));
    assert!(slot.current_id().is_none());
}

#[test]
fn matching_finish_releases_exactly_one_registered_run() {
    let mut slot = TaskSlot::default();
    let request = slot.start("A");
    let run = Arc::new(CountingRun::default());
    let registration = request.token.register(run.clone()).unwrap();
    assert!(slot.finish(request.id));
    assert!(!slot.finish(request.id));
    assert_eq!(run.calls(), 1);
    assert!(request.token.is_cancelled());
    assert!(slot.key().is_none());
    drop(registration);
}

#[test]
fn unrelated_finish_does_not_cancel_current_task() {
    let mut slot = TaskSlot::default();
    let request = slot.start("A");
    let run = Arc::new(CountingRun::default());
    let registration = request.token.register(run.clone()).unwrap();
    assert!(!slot.finish(request.id + 1));
    assert_eq!(run.calls(), 0);
    assert!(slot.owns(request.id));
    request
        .token
        .checkpoint()
        .expect("错误身份不能取消当前请求");
    drop(registration);
}

#[test]
fn cancelling_one_slot_does_not_touch_another_slot() {
    let mut slot_a = TaskSlot::default();
    let mut slot_b = TaskSlot::default();
    let request_a = slot_a.start("A");
    let request_b = slot_b.start("B");
    let run_a = Arc::new(CountingRun::default());
    let run_b = Arc::new(CountingRun::default());
    let registration_a = request_a.token.register(run_a.clone()).unwrap();
    let registration_b = request_b.token.register(run_b.clone()).unwrap();
    assert!(slot_a.cancel("关闭 A"));
    assert!(request_a.token.is_cancelled());
    assert!(slot_b.owns(request_b.id));
    request_b.token.checkpoint().expect("取消 A 不能取消 B");
    assert_eq!(run_a.calls(), 1);
    assert_eq!(run_b.calls(), 0);
    drop(registration_a);
    drop(registration_b);
}

#[test]
fn dropping_slot_cancels_live_worker_as_fallback() {
    let mut slot = TaskSlot::default();
    let request = slot.start("A");
    let run = Arc::new(CountingRun::default());
    let registration = request.token.register(run.clone()).unwrap();
    drop(slot);
    assert!(request.token.is_cancelled());
    assert_eq!(run.calls(), 1);
    drop(registration);
}

#[test]
fn dropping_request_copies_cannot_cancel_slot_owner() {
    let mut slot = TaskSlot::default();
    let request = slot.start("A");
    let worker = request.clone();
    drop(request);
    drop(slot.token());
    drop(worker.token.clone());
    worker.token.checkpoint().expect("工作副本析构不能取消宿主");
    assert!(slot.owns(worker.id));
}

#[test]
fn repeated_slot_cancel_controls_run_once() {
    let mut slot = TaskSlot::default();
    let request = slot.start("A");
    let run = Arc::new(CountingRun::default());
    let registration = request.token.register(run.clone()).unwrap();
    assert!(slot.cancel("关闭窗口"));
    assert!(!slot.cancel("再次关闭"));
    drop(slot);
    assert_eq!(run.calls(), 1);
    assert!(request.token.is_cancelled());
    drop(registration);
}

#[test]
fn typed_cancel_receipt_is_classified_before_error_string() {
    let result: crate::Result<()> = Err(OcrCancelled.into());
    assert_eq!(Receipt::from_result(result), Receipt::Cancelled);
    let text_error: crate::Result<()> = Err("OCR 任务已取消".into());
    assert_eq!(
        Receipt::from_result(text_error),
        Receipt::Failed("OCR 任务已取消".into()),
        "字符串相同不具有专门取消类型"
    );
}

#[test]
fn genuine_worker_failure_remains_failure_when_owner_already_cancelled() {
    let mut slot = TaskSlot::default();
    let request = slot.start("A");
    slot.cancel("源失效");
    assert!(request.token.is_cancelled());
    let result: crate::Result<()> = Err(RealFailure.into());
    assert_eq!(
        Receipt::from_result(result),
        Receipt::Failed("模型输出真实失败".into())
    );
}

#[test]
fn mapping_receipt_transforms_only_success() {
    assert_eq!(
        Receipt::Completed(4).map(|value| value + 1),
        Receipt::Completed(5)
    );
    assert_eq!(
        Receipt::<usize>::Cancelled.map(|_| panic!("取消不执行成功映射")),
        Receipt::<()>::Cancelled
    );
    assert_eq!(
        Receipt::<usize>::Failed("原错".into()).map(|_| panic!("失败不执行成功映射")),
        Receipt::<()>::Failed("原错".into())
    );
}

#[test]
fn control_failure_clears_only_owned_slot_without_repeating_terminate() {
    let mut slot = TaskSlot::default();
    let request = slot.start("A");
    let run = Arc::new(FailingRun::default());
    let registration = request.token.register(run.clone()).unwrap();
    assert!(slot.cancel("ROI 更换"));
    assert!(request.token.is_cancelled());
    assert!(slot.key().is_none());
    assert!(!slot.cancel("重试取消"));
    drop(slot);
    assert_eq!(run.calls.load(Ordering::Acquire), 1);
    drop(registration);
}

#[test]
fn completion_release_preserves_receipt_even_if_cancel_control_fails() {
    let mut slot = TaskSlot::default();
    let request = slot.start("A");
    let run = Arc::new(FailingRun::default());
    let registration = request.token.register(run.clone()).unwrap();
    assert_eq!(
        slot.finish_receipt(request.id, Receipt::Completed("已完成")),
        Some(Receipt::Completed("已完成"))
    );
    assert_eq!(run.calls.load(Ordering::Acquire), 1);
    assert!(request.token.is_cancelled());
    drop(registration);
}

#[test]
fn control_failure_log_names_request_and_does_not_claim_run_exit() {
    let message = cancellation_failure_message(19, "任务槽析构兜底", &RealFailure);
    assert!(message.contains("id=19"));
    assert!(message.contains("reason=任务槽析构兜底"));
    assert!(message.contains("模型输出真实失败"));
    assert!(message.contains("已登记取消，未确认原生推理已退出"));
}

#[test]
fn exhausted_identity_panics_before_cancelling_live_request() {
    let mut slot = TaskSlot::default();
    let request = slot.start("A");
    slot.serial = u64::MAX;
    let overflow = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| slot.start("B")));
    assert!(overflow.is_err());
    assert!(slot.owns(request.id));
    assert_eq!(slot.key(), Some(&"A"));
    request.token.checkpoint().expect("身份耗尽不得先取消 A");
}
