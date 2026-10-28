//! 原始 status 捕获和取消分类的纯模型；不构造 RunOptions、不加载 ORT。
use super::*;

const OPTIONS_A: usize = 0x1000;
const OPTIONS_B: usize = 0x2000;

fn failure(options_identity: usize) -> RawRunFailure {
    RawRunFailure {
        options_identity,
        status_identity: 0x3000,
        code: ort::sys::OrtErrorCode::ORT_FAIL as i32,
        message: Some(TERMINATE_MESSAGE.to_owned()),
    }
}

fn matches(raw: Option<&RawRunFailure>) -> bool {
    matches_task_termination(
        true,
        true,
        OPTIONS_A,
        raw,
        ort::ErrorCode::GenericFailure,
        TERMINATE_MESSAGE,
    )
}

#[test]
fn cancellation_requires_task_acceptance_and_matching_native_options() {
    let raw = failure(OPTIONS_A);
    assert!(matches(Some(&raw)));
    for (task_cancelled, termination_accepted) in [(false, false), (false, true), (true, false)] {
        assert!(!matches_task_termination(
            task_cancelled,
            termination_accepted,
            OPTIONS_A,
            Some(&raw),
            ort::ErrorCode::GenericFailure,
            TERMINATE_MESSAGE,
        ));
    }
    assert!(!matches(None));
    assert!(!matches(Some(&failure(OPTIONS_B))));
    let mut empty_status = failure(OPTIONS_A);
    empty_status.status_identity = 0;
    assert!(!matches(Some(&empty_status)));
    assert!(!matches_task_termination(
        true,
        true,
        0,
        Some(&failure(0)),
        ort::ErrorCode::GenericFailure,
        TERMINATE_MESSAGE,
    ));
}

#[test]
fn only_exact_native_fail_and_message_match_despite_wrapper_generic_failure() {
    for code in [
        12, // 固定原生 1.24.4 的 ORT_MODEL_LOAD_CANCELED，Rust 包未声明。
        ort::sys::OrtErrorCode::ORT_RUNTIME_EXCEPTION as i32,
        ort::sys::OrtErrorCode::ORT_INVALID_ARGUMENT as i32,
        13, // 固定原生 1.24.4 的 ORT_MODEL_REQUIRES_COMPILATION。
    ] {
        let mut raw = failure(OPTIONS_A);
        raw.code = code;
        assert!(!matches(Some(&raw)));
    }
    for message in [
        "模型加载失败",
        "Exiting due to terminate flag being set to true",
        "Failed to run inference on model: Exiting due to terminate flag being set to true.",
        "Exiting due to terminate flag being set to true. 另一个错误",
    ] {
        let mut raw = failure(OPTIONS_A);
        raw.message = Some(message.to_owned());
        assert!(!matches(Some(&raw)));
        assert!(!matches_task_termination(
            true,
            true,
            OPTIONS_A,
            Some(&failure(OPTIONS_A)),
            ort::ErrorCode::GenericFailure,
            message,
        ));
    }
    let mut invalid_utf8_or_null_message = failure(OPTIONS_A);
    invalid_utf8_or_null_message.message = None;
    assert!(!matches(Some(&invalid_utf8_or_null_message)));
    assert!(!matches_task_termination(
        true,
        true,
        OPTIONS_A,
        Some(&failure(OPTIONS_A)),
        ort::ErrorCode::RuntimeException,
        TERMINATE_MESSAGE,
    ));
}

#[test]
fn capture_is_bound_to_one_options_instance_and_one_run() {
    let guard = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
    assert!(capture_interested(OPTIONS_A));
    assert!(!capture_interested(OPTIONS_B));
    record_run_status(OPTIONS_B, Some(failure(OPTIONS_B)));
    record_run_status(OPTIONS_A, Some(failure(OPTIONS_A)));
    let raw = guard.finish().expect("匹配选项的一次 Run 有原始 status");
    assert_eq!(raw.options_identity, OPTIONS_A);
    assert_eq!(raw.status_identity, 0x3000);
    assert_eq!(raw.code, ort::sys::OrtErrorCode::ORT_FAIL as i32);
    assert!(matches(Some(&raw)));
    assert!(!capture_interested(OPTIONS_A));

    let guard = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
    record_run_status(OPTIONS_A, Some(failure(OPTIONS_A)));
    record_run_status(OPTIONS_A, Some(failure(OPTIONS_A)));
    assert!(
        guard.finish().is_none(),
        "多次 Run 不能借用最后一次错误身份"
    );
}

#[test]
fn pre_run_errors_success_and_wrong_options_have_no_cancel_status() {
    let guard = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
    assert!(
        guard.finish().is_none(),
        "包装层在 Run 前失败没有原始 Run status"
    );
    let guard = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
    record_run_status(OPTIONS_A, None);
    assert!(guard.finish().is_none(), "成功 Run 不生成取消失败快照");
    let guard = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
    record_run_status(OPTIONS_B, Some(failure(OPTIONS_B)));
    assert!(guard.finish().is_none(), "其它任务不能提供原始取消 status");
}

#[test]
fn nested_capture_is_rejected_without_replacing_active_owner() {
    let guard = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
    assert!(RunOutcomeGuard::begin(OPTIONS_B).is_err());
    assert!(capture_interested(OPTIONS_A));
    assert!(!capture_interested(OPTIONS_B));
    record_run_status(OPTIONS_A, Some(failure(OPTIONS_A)));
    assert!(matches(guard.finish().as_ref()));
}

#[test]
fn dropped_or_unwound_capture_cannot_cancel_the_next_task() {
    let old = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
    record_run_status(OPTIONS_A, Some(failure(OPTIONS_A)));
    drop(old);
    assert!(!capture_interested(OPTIONS_A));
    let next = RunOutcomeGuard::begin(OPTIONS_B).unwrap();
    record_run_status(OPTIONS_A, Some(failure(OPTIONS_A)));
    assert!(next.finish().is_none());

    let unwind = std::panic::catch_unwind(|| {
        let _guard = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
        record_run_status(OPTIONS_A, Some(failure(OPTIONS_A)));
        panic!("测试模拟包装层异常退出");
    });
    assert!(unwind.is_err());
    assert!(!capture_interested(OPTIONS_A));
    assert!(RunOutcomeGuard::begin(OPTIONS_B)
        .unwrap()
        .finish()
        .is_none());
}

#[test]
fn thread_local_capture_never_accepts_another_threads_status() {
    let guard = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
    let other_thread_interested = std::thread::spawn(|| {
        record_run_status(OPTIONS_A, Some(failure(OPTIONS_A)));
        capture_interested(OPTIONS_A)
    })
    .join()
    .unwrap();
    assert!(!other_thread_interested);
    assert!(guard.finish().is_none());
}

#[test]
fn stale_guard_does_not_release_a_new_capture_generation() {
    let stale = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
    RUN_CAPTURE.with(|slot| slot.borrow_mut().active = None);
    let fresh = RunOutcomeGuard::begin(OPTIONS_A).unwrap();
    drop(stale);
    assert!(capture_interested(OPTIONS_A));
    record_run_status(OPTIONS_A, Some(failure(OPTIONS_A)));
    assert!(matches(fresh.finish().as_ref()));
}
