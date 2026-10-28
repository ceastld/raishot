//! 原始整数错误码桥接的纯模型；仅局部假状态回调，不发布 API、不加载 ORT。
use super::*;
use std::{
    ffi::{CStr, CString},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

const OPTIONS: usize = 0x1000;
static INVALID_NATIVE_CALLS: AtomicUsize = AtomicUsize::new(0);
static INVALID_CALL_TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct Calls {
    code: AtomicUsize,
    message: AtomicUsize,
    released: AtomicUsize,
    release_diagnostic: Mutex<Option<(usize, Vec<u8>)>>,
    release_saw_live_diagnostic: AtomicBool,
    release_cache: Mutex<Option<usize>>,
    release_saw_unlocked_detached_cache: AtomicBool,
}

struct FakeStatus {
    raw_code: i32,
    message: Option<CString>,
    calls: Arc<Calls>,
    release_frees: bool,
}

struct FakeStatusOwner {
    pointer: *mut ort::sys::OrtStatus,
    calls: Arc<Calls>,
    release_frees: bool,
}

impl FakeStatusOwner {
    fn new(raw_code: i32, message: Option<&[u8]>) -> Self {
        Self::with_release_policy(raw_code, message, true)
    }

    // 原生分配器可复用已释放状态的地址；这里保留同一槽位模拟相同地址再分配。
    fn reusable(raw_code: i32, message: Option<&[u8]>) -> Self {
        Self::with_release_policy(raw_code, message, false)
    }

    fn with_release_policy(raw_code: i32, message: Option<&[u8]>, release_frees: bool) -> Self {
        let calls = Arc::new(Calls::default());
        let status = Box::new(FakeStatus {
            raw_code,
            message: message.map(|bytes| CString::new(bytes).expect("假消息不含 NUL")),
            calls: Arc::clone(&calls),
            release_frees,
        });
        Self {
            pointer: Box::into_raw(status).cast(),
            calls,
            release_frees,
        }
    }

    fn pointer(&self) -> *const ort::sys::OrtStatus {
        self.pointer
    }

    fn message_pointer(&self) -> *const c_char {
        unsafe { &*self.pointer.cast::<FakeStatus>() }
            .message
            .as_ref()
            .map_or(std::ptr::null(), |message| message.as_ptr())
    }

    fn reset_reused_slot(&mut self, raw_code: i32, message: &[u8]) {
        assert!(!self.release_frees, "仅保留的假状态槽位可以复用");
        let status = unsafe { &mut *self.pointer.cast::<FakeStatus>() };
        status.raw_code = raw_code;
        status.message = Some(CString::new(message).expect("假消息不含 NUL"));
    }

    fn release(&mut self, api: &NativeStatusApi, messages: &StatusMessages) {
        let before = self.calls.released.load(Ordering::SeqCst);
        unsafe { release_status(Some(api), messages, self.pointer) };
        if self.release_frees && self.calls.released.load(Ordering::SeqCst) > before {
            self.pointer = std::ptr::null_mut();
        }
    }
}

impl Drop for FakeStatusOwner {
    fn drop(&mut self) {
        if !self.pointer.is_null()
            && (!self.release_frees || self.calls.released.load(Ordering::SeqCst) == 0)
        {
            drop(unsafe { Box::from_raw(self.pointer.cast::<FakeStatus>()) });
        }
    }
}

fn invalid_native_status(status: *const ort::sys::OrtStatus) -> bool {
    if status.is_null() || status == bridge_unavailable_status().0.cast_const() {
        INVALID_NATIVE_CALLS.fetch_add(1, Ordering::SeqCst);
        true
    } else {
        false
    }
}

unsafe extern "system" fn fake_error_code(status: *const ort::sys::OrtStatus) -> i32 {
    if invalid_native_status(status) {
        return i32::MIN;
    }
    let status = unsafe { &*status.cast::<FakeStatus>() };
    status.calls.code.fetch_add(1, Ordering::SeqCst);
    status.raw_code
}

unsafe extern "system" fn fake_error_message(status: *const ort::sys::OrtStatus) -> *const c_char {
    if invalid_native_status(status) {
        return c"假原生回调不应收到该状态".as_ptr();
    }
    let status = unsafe { &*status.cast::<FakeStatus>() };
    status.calls.message.fetch_add(1, Ordering::SeqCst);
    status
        .message
        .as_ref()
        .map_or(std::ptr::null(), |message| message.as_ptr())
}

unsafe extern "system" fn fake_release_status(status: *mut ort::sys::OrtStatus) {
    if invalid_native_status(status) {
        return;
    }
    let fake = unsafe { &*status.cast::<FakeStatus>() };
    if let Some((pointer, expected)) = fake.calls.release_diagnostic.lock().as_ref() {
        let actual = unsafe { CStr::from_ptr(*pointer as *const c_char) }.to_bytes();
        fake.calls
            .release_saw_live_diagnostic
            .store(actual == expected.as_slice(), Ordering::SeqCst);
    }
    if let Some(pointer) = *fake.calls.release_cache.lock() {
        // 此地址只在持有局部 messages 的同步 release 调用期间提供。
        let messages = unsafe { &*(pointer as *const StatusMessages) };
        let detached_and_unlocked = messages
            .messages
            .try_lock()
            .is_some_and(|entries| !entries.contains_key(&(status as usize)));
        fake.calls
            .release_saw_unlocked_detached_cache
            .store(detached_and_unlocked, Ordering::SeqCst);
    }
    fake.calls.released.fetch_add(1, Ordering::SeqCst);
    if fake.release_frees {
        drop(unsafe { Box::from_raw(status.cast::<FakeStatus>()) });
    }
}

fn fake_api() -> NativeStatusApi {
    NativeStatusApi {
        error_code: fake_error_code,
        error_message: fake_error_message,
        release_status: fake_release_status,
    }
}

fn message_text(pointer: *const c_char) -> String {
    assert!(!pointer.is_null(), "桥接消息必须是有效非空指针");
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .expect("桥接诊断应为有效 UTF-8")
        .to_owned()
}

// 对照 rc.13 Error::result_from_status 的指针合同；不构造 ort::Error，
// 因而不调用 CreateStatus，也不安装任何全局测试 API。
unsafe fn model_wrapper_result(
    api: Option<&NativeStatusApi>,
    messages: &StatusMessages,
    status: *const ort::sys::OrtStatus,
) -> std::result::Result<(), (ort::ErrorCode, String)> {
    if status.is_null() {
        return Ok(());
    }
    let code = unsafe { status_error_code(api, status) };
    let message = unsafe { status_error_message(api, messages, status) };
    Err((ort::ErrorCode::from(code), message_text(message)))
}

#[test]
fn declared_codes_keep_all_twelve_binding_discriminants() {
    use ort::sys::OrtErrorCode::*;
    let expected = [
        ORT_OK,
        ORT_FAIL,
        ORT_INVALID_ARGUMENT,
        ORT_NO_SUCHFILE,
        ORT_NO_MODEL,
        ORT_ENGINE_ERROR,
        ORT_RUNTIME_EXCEPTION,
        ORT_INVALID_PROTOBUF,
        ORT_MODEL_LOADED,
        ORT_NOT_IMPLEMENTED,
        ORT_INVALID_GRAPH,
        ORT_EP_FAIL,
    ];
    for (raw, expected) in expected.into_iter().enumerate() {
        assert_eq!(declared_error_code(raw as i32), expected);
    }
}

#[test]
fn api24_new_codes_are_safe_failures_with_raw_named_diagnostics() {
    let api = fake_api();
    let messages = StatusMessages::default();
    for (raw, name) in [
        (12, "ORT_MODEL_LOAD_CANCELED"),
        (13, "ORT_MODEL_REQUIRES_COMPILATION"),
        (14, "ORT_NOT_FOUND"),
    ] {
        let mut status = FakeStatusOwner::new(raw, Some("原始加载失败".as_bytes()));
        let error = unsafe { model_wrapper_result(Some(&api), &messages, status.pointer()) }
            .expect_err("新增原始码必须保留为真实错误");
        assert_eq!(error.0, ort::ErrorCode::GenericFailure);
        assert!(error.1.contains(&raw.to_string()));
        assert!(error.1.contains(name));
        assert!(error.1.contains("原始加载失败"));
        status.release(&api, &messages);
        assert_eq!(status.calls.released.load(Ordering::SeqCst), 1);
    }
    assert!(messages.messages.lock().is_empty());
}

#[test]
fn future_unknown_positive_negative_and_extreme_codes_are_real_failures() {
    let api = fake_api();
    let messages = StatusMessages::default();
    for raw in [15, 99, i32::MAX, -1, -12, i32::MIN] {
        assert_eq!(declared_error_code(raw), ort::sys::OrtErrorCode::ORT_FAIL);
        let mut status = FakeStatusOwner::new(raw, Some("未知码原始诊断".as_bytes()));
        let error = unsafe { model_wrapper_result(Some(&api), &messages, status.pointer()) }
            .expect_err("未知整数不能变为成功");
        assert_eq!(error.0, ort::ErrorCode::GenericFailure);
        assert!(error.1.contains(&raw.to_string()));
        assert!(error.1.contains("未知"));
        assert!(error.1.contains("未知码原始诊断"));
        status.release(&api, &messages);
    }
}

#[test]
fn null_status_bypasses_native_callbacks_and_remains_success() {
    let _serial = INVALID_CALL_TEST_LOCK.lock();
    INVALID_NATIVE_CALLS.store(0, Ordering::SeqCst);
    let api = fake_api();
    let messages = StatusMessages::default();
    let null = std::ptr::null();
    assert_eq!(
        unsafe { status_error_code(Some(&api), null) },
        ort::sys::OrtErrorCode::ORT_OK
    );
    let message = unsafe { status_error_message(Some(&api), &messages, null) };
    assert!(!message_text(message).is_empty());
    assert!(unsafe { model_wrapper_result(Some(&api), &messages, null) }.is_ok());
    unsafe { release_status(Some(&api), &messages, std::ptr::null_mut()) };
    assert_eq!(INVALID_NATIVE_CALLS.load(Ordering::SeqCst), 0);
    assert!(messages.messages.lock().is_empty());
}

#[test]
fn unavailable_bridge_marker_is_non_null_error_without_native_callbacks() {
    let _serial = INVALID_CALL_TEST_LOCK.lock();
    INVALID_NATIVE_CALLS.store(0, Ordering::SeqCst);
    let api = fake_api();
    let messages = StatusMessages::default();
    let marker = bridge_unavailable_status();
    assert!(!marker.0.is_null(), "未初始化不能返回伪成功状态");
    assert_eq!(marker.0, bridge_unavailable_status().0);
    for installed in [None, Some(&api)] {
        let error = unsafe { model_wrapper_result(installed, &messages, marker.0) }
            .expect_err("静态桥接标记必须保留错误");
        assert_eq!(error.0, ort::ErrorCode::GenericFailure);
        assert!(!error.1.is_empty());
        unsafe { release_status(installed, &messages, marker.0) };
    }
    assert_eq!(INVALID_NATIVE_CALLS.load(Ordering::SeqCst), 0);
    assert!(messages.messages.lock().is_empty());
}

#[test]
fn missing_native_api_keeps_non_null_status_as_error() {
    let messages = StatusMessages::default();
    let status = FakeStatusOwner::new(12, Some("未发布 API 的状态".as_bytes()));
    let error = unsafe { model_wrapper_result(None, &messages, status.pointer()) }
        .expect_err("非空状态不能因桥接未发布而消失");
    assert_eq!(error.0, ort::ErrorCode::GenericFailure);
    assert!(!error.1.is_empty());
    unsafe { release_status(None, &messages, status.pointer) };
    assert_eq!(status.calls.code.load(Ordering::SeqCst), 0);
    assert_eq!(status.calls.message.load(Ordering::SeqCst), 0);
    assert_eq!(status.calls.released.load(Ordering::SeqCst), 0);
}

#[test]
fn non_null_raw_zero_stays_error_even_when_code_is_ok() {
    let api = fake_api();
    let messages = StatusMessages::default();
    let mut status = FakeStatusOwner::new(0, Some("非空状态实际错误".as_bytes()));
    let error = unsafe { model_wrapper_result(Some(&api), &messages, status.pointer()) }
        .expect_err("状态成功只认空指针");
    assert_eq!(error.0, ort::ErrorCode::Ok);
    assert_eq!(error.1, "非空状态实际错误");
    status.release(&api, &messages);
}

#[test]
fn known_native_messages_are_forwarded_unchanged_until_release() {
    let api = fake_api();
    let messages = StatusMessages::default();
    for raw in 0..=11 {
        let mut status = FakeStatusOwner::new(raw, Some("真实错误原文".as_bytes()));
        let original = status.message_pointer();
        assert_eq!(
            unsafe { status_error_code(Some(&api), status.pointer()) } as i32,
            raw
        );
        let forwarded = unsafe { status_error_message(Some(&api), &messages, status.pointer()) };
        assert_eq!(forwarded, original);
        assert_eq!(message_text(forwarded), "真实错误原文");
        assert!(messages.messages.lock().is_empty());
        status.release(&api, &messages);
        assert_eq!(status.calls.released.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn unknown_message_can_be_read_before_error_code() {
    let api = fake_api();
    let messages = StatusMessages::default();
    let mut status = FakeStatusOwner::new(14, Some("消息先读".as_bytes()));
    let message = unsafe { status_error_message(Some(&api), &messages, status.pointer()) };
    let text = message_text(message);
    assert!(text.contains("14"));
    assert!(text.contains("ORT_NOT_FOUND"));
    assert!(text.contains("消息先读"));
    assert_eq!(
        unsafe { status_error_code(Some(&api), status.pointer()) },
        ort::sys::OrtErrorCode::ORT_FAIL
    );
    status.release(&api, &messages);
}

#[test]
fn cached_diagnostic_pointer_and_content_are_stable_on_repeated_reads() {
    let api = fake_api();
    let messages = StatusMessages::default();
    let mut status = FakeStatusOwner::new(12, Some("首次诊断".as_bytes()));
    let first = unsafe { status_error_message(Some(&api), &messages, status.pointer()) };
    let first_text = message_text(first);
    // 同一活状态的旁表条目不能被再次读消息替换，避免旧返回指针悬垂。
    unsafe { &mut *status.pointer.cast::<FakeStatus>() }.message =
        Some(CString::new("随后不同的原生诊断").unwrap());
    for _ in 0..4 {
        let next = unsafe { status_error_message(Some(&api), &messages, status.pointer()) };
        assert_eq!(next, first);
        assert_eq!(message_text(next), first_text);
    }
    assert!(first_text.contains("首次诊断"));
    status.release(&api, &messages);
}

#[test]
fn null_native_message_gets_non_empty_owned_diagnostic_with_raw_code() {
    let api = fake_api();
    let messages = StatusMessages::default();
    for raw in [1, 12, 14, -1] {
        let mut status = FakeStatusOwner::new(raw, None);
        let message = unsafe { status_error_message(Some(&api), &messages, status.pointer()) };
        let text = message_text(message);
        assert!(text.contains(&raw.to_string()));
        assert!(text.contains("空"));
        assert!(messages
            .messages
            .lock()
            .contains_key(&(status.pointer() as usize)));
        status.release(&api, &messages);
    }
    assert!(messages.messages.lock().is_empty());
}

#[test]
fn invalid_utf8_native_message_still_preserves_unknown_integer_diagnostic() {
    let api = fake_api();
    let messages = StatusMessages::default();
    let mut status = FakeStatusOwner::new(13, Some(&[b'A', 0xff, b'B']));
    let message = unsafe { status_error_message(Some(&api), &messages, status.pointer()) };
    let text = message_text(message);
    assert!(text.contains("13"));
    assert!(text.contains("ORT_MODEL_REQUIRES_COMPILATION"));
    assert!(text.contains('A') && text.contains('B'));
    status.release(&api, &messages);
}

#[test]
fn two_live_statuses_have_separate_diagnostics_and_release_ownership() {
    let api = fake_api();
    let messages = StatusMessages::default();
    let mut first = FakeStatusOwner::new(12, Some("状态 A".as_bytes()));
    let mut second = FakeStatusOwner::new(13, Some("状态 B".as_bytes()));
    let first_key = first.pointer() as usize;
    let second_key = second.pointer() as usize;
    let first_message = unsafe { status_error_message(Some(&api), &messages, first.pointer()) };
    let second_message = unsafe { status_error_message(Some(&api), &messages, second.pointer()) };
    assert_ne!(first_message, second_message);
    assert!(message_text(first_message).contains("状态 A"));
    assert!(message_text(second_message).contains("状态 B"));
    assert_eq!(messages.messages.lock().len(), 2);
    first.release(&api, &messages);
    assert!(!messages.messages.lock().contains_key(&first_key));
    assert!(messages.messages.lock().contains_key(&second_key));
    assert_eq!(second.calls.released.load(Ordering::SeqCst), 0);
    assert!(message_text(second_message).contains("状态 B"));
    second.release(&api, &messages);
    assert!(messages.messages.lock().is_empty());
}

#[test]
fn release_retains_diagnostic_during_native_callback_then_clears_cache() {
    let api = fake_api();
    let messages = StatusMessages::default();
    let mut status = FakeStatusOwner::new(12, Some("释放期间仍有效".as_bytes()));
    let key = status.pointer() as usize;
    let message = unsafe { status_error_message(Some(&api), &messages, status.pointer()) };
    *status.calls.release_diagnostic.lock() = Some((
        message as usize,
        unsafe { CStr::from_ptr(message) }.to_bytes().to_owned(),
    ));
    *status.calls.release_cache.lock() = Some(&messages as *const StatusMessages as usize);
    status.release(&api, &messages);
    assert_eq!(status.calls.released.load(Ordering::SeqCst), 1);
    assert!(status
        .calls
        .release_saw_live_diagnostic
        .load(Ordering::SeqCst));
    assert!(status
        .calls
        .release_saw_unlocked_detached_cache
        .load(Ordering::SeqCst));
    assert!(!messages.messages.lock().contains_key(&key));
}

#[test]
fn released_pointer_reuse_gets_new_code_and_message_without_stale_cache() {
    let api = fake_api();
    let messages = StatusMessages::default();
    let mut slot = FakeStatusOwner::reusable(12, Some("旧状态诊断".as_bytes()));
    let key = slot.pointer() as usize;
    let old = unsafe { status_error_message(Some(&api), &messages, slot.pointer()) };
    assert!(message_text(old).contains("旧状态诊断"));
    slot.release(&api, &messages);
    assert!(!messages.messages.lock().contains_key(&key));
    slot.reset_reused_slot(14, "新状态诊断".as_bytes());
    assert_eq!(slot.pointer() as usize, key);
    let new = unsafe { status_error_message(Some(&api), &messages, slot.pointer()) };
    let text = message_text(new);
    assert!(text.contains("14"));
    assert!(text.contains("ORT_NOT_FOUND"));
    assert!(text.contains("新状态诊断"));
    assert!(!text.contains("旧状态诊断"));
    slot.release(&api, &messages);
    assert_eq!(slot.calls.released.load(Ordering::SeqCst), 2);
    assert!(messages.messages.lock().is_empty());
}

#[test]
fn diagnostic_status_can_be_released_from_another_thread() {
    let api = fake_api();
    let messages = Arc::new(StatusMessages::default());
    let mut status = FakeStatusOwner::new(13, Some("跨线程释放".as_bytes()));
    let key = status.pointer() as usize;
    let message = unsafe { status_error_message(Some(&api), &messages, status.pointer()) };
    let copied = message_text(message);
    let shared = Arc::clone(&messages);
    std::thread::spawn(move || unsafe {
        release_status(Some(&api), &shared, key as *mut ort::sys::OrtStatus)
    })
    .join()
    .unwrap();
    if status.calls.released.load(Ordering::SeqCst) > 0 {
        status.pointer = std::ptr::null_mut();
    }
    assert_eq!(status.calls.released.load(Ordering::SeqCst), 1);
    assert!(!messages.messages.lock().contains_key(&key));
    assert!(copied.contains("13") && copied.contains("跨线程释放"));
}

#[test]
fn safe_remapping_of_unknown_raw_codes_cannot_prove_native_cancellation() {
    for raw in [12, 13, 14, 15, i32::MAX, -1, i32::MIN] {
        let safe = ort::ErrorCode::from(declared_error_code(raw));
        assert_eq!(safe, ort::ErrorCode::GenericFailure);
        let failure = RawRunFailure {
            options_identity: OPTIONS,
            status_identity: 0x2000,
            code: raw,
            message: Some(TERMINATE_MESSAGE.to_owned()),
        };
        assert!(!matches_task_termination(
            true,
            true,
            OPTIONS,
            Some(&failure),
            safe,
            TERMINATE_MESSAGE,
        ));
    }
}

#[test]
fn true_raw_native_fail_remains_eligible_for_exact_cancellation() {
    let failure = RawRunFailure {
        options_identity: OPTIONS,
        status_identity: 0x2000,
        code: 1,
        message: Some(TERMINATE_MESSAGE.to_owned()),
    };
    assert!(matches_task_termination(
        true,
        true,
        OPTIONS,
        Some(&failure),
        ort::ErrorCode::from(declared_error_code(1)),
        TERMINATE_MESSAGE,
    ));
}

#[test]
fn known_non_fail_statuses_keep_real_errors_even_with_terminate_message() {
    let api = fake_api();
    let messages = StatusMessages::default();
    for raw in [2, 3, 5, 6, 9, 10, 11] {
        let mut status = FakeStatusOwner::new(raw, Some(TERMINATE_MESSAGE.as_bytes()));
        let error = unsafe { model_wrapper_result(Some(&api), &messages, status.pointer()) }
            .expect_err("非空已知状态仍须返回原始错误");
        assert_eq!(error.1, TERMINATE_MESSAGE);
        assert_ne!(error.0, ort::ErrorCode::GenericFailure);
        let failure = RawRunFailure {
            options_identity: OPTIONS,
            status_identity: status.pointer() as usize,
            code: raw,
            message: Some(TERMINATE_MESSAGE.to_owned()),
        };
        assert!(!matches_task_termination(
            true,
            true,
            OPTIONS,
            Some(&failure),
            error.0,
            &error.1,
        ));
        status.release(&api, &messages);
    }
}
