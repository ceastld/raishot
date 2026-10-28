//! 固定 CPU 原生库的显式 API24 加载；加载／协商错误正常返回，不走 panic。
use super::{ModelRunner, Tensor};
use crate::{
    recognition::task::{OcrCancelled, RunCancellation, RunRegistration, TaskToken},
    Result,
};
use libloading::os::windows::{
    Library, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use ort::{
    session::{builder::GraphOptimizationLevel, RunOptions, Session, SessionOutputs},
    AsPointer,
};
use parking_lot::Mutex;
use std::{
    cell::RefCell,
    collections::HashMap,
    ffi::{c_char, CStr, CString},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::HMODULE,
        System::LibraryLoader::{
            GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        },
    },
};

type NativeRun = unsafe extern "system" fn(
    *mut ort::sys::OrtSession,
    *const ort::sys::OrtRunOptions,
    *const *const c_char,
    *const *const ort::sys::OrtValue,
    usize,
    *const *const c_char,
    usize,
    *mut *mut ort::sys::OrtValue,
) -> ort::sys::OrtStatusPtr;

#[derive(Clone, Copy)]
struct NativeStatusApi {
    error_code: unsafe extern "system" fn(*const ort::sys::OrtStatus) -> i32,
    error_message: unsafe extern "system" fn(*const ort::sys::OrtStatus) -> *const c_char,
    release_status: unsafe extern "system" fn(*mut ort::sys::OrtStatus),
}

struct NativeRunApi {
    run: NativeRun,
    status: NativeStatusApi,
    // 原函数固化后立即持有 DLL；发布失败也不会留下指向已卸载库的静态指针。
    _library: Library,
}
static NATIVE_RUN_API: OnceLock<NativeRunApi> = OnceLock::new();

#[derive(Default)]
struct StatusMessages {
    messages: Mutex<HashMap<usize, CString>>,
}
impl StatusMessages {
    fn remember(&self, identity: usize, message: CString) -> *const c_char {
        self.messages
            .lock()
            .entry(identity)
            .or_insert(message)
            .as_ptr()
    }

    fn detach(&self, identity: usize) -> Option<CString> {
        self.messages.lock().remove(&identity)
    }
}
static STATUS_MESSAGES: OnceLock<StatusMessages> = OnceLock::new();

// 防御性非空错误标记：三个状态 shim 自己消费，绝不交给原生 API 解引用或释放。
static UNAVAILABLE_STATUS: u8 = 0;
fn bridge_unavailable_status() -> ort::sys::OrtStatusPtr {
    ort::sys::OrtStatusPtr((&UNAVAILABLE_STATUS as *const u8).cast_mut().cast())
}
fn is_bridge_status(status: *const ort::sys::OrtStatus) -> bool {
    status == bridge_unavailable_status().0.cast_const()
}

// 只有已声明的判别值可以构造 Rust 枚举；其余原始整数保留在消息诊断中。
fn declared_error_code(raw: i32) -> ort::sys::OrtErrorCode {
    use ort::sys::OrtErrorCode::*;
    match raw {
        0 => ORT_OK,
        1 => ORT_FAIL,
        2 => ORT_INVALID_ARGUMENT,
        3 => ORT_NO_SUCHFILE,
        4 => ORT_NO_MODEL,
        5 => ORT_ENGINE_ERROR,
        6 => ORT_RUNTIME_EXCEPTION,
        7 => ORT_INVALID_PROTOBUF,
        8 => ORT_MODEL_LOADED,
        9 => ORT_NOT_IMPLEMENTED,
        10 => ORT_INVALID_GRAPH,
        11 => ORT_EP_FAIL,
        _ => ORT_FAIL,
    }
}

fn unknown_status_message(raw: i32, message: Option<&CStr>) -> CString {
    let name = match raw {
        12 => "ORT_MODEL_LOAD_CANCELED",
        13 => "ORT_MODEL_REQUIRES_COMPILATION",
        14 => "ORT_NOT_FOUND",
        _ => "未知或异常原生错误码",
    };
    let original = message.map(CStr::to_string_lossy);
    let text = format!(
        "ORT 原始错误码 {raw}（{name}）：{}",
        original.as_deref().unwrap_or("原生错误消息为空")
    );
    // 前缀没有 NUL，CStr::to_string_lossy 的内容也没有内部 NUL。
    unsafe { CString::from_vec_unchecked(text.into_bytes()) }
}

unsafe fn status_error_code(
    api: Option<&NativeStatusApi>,
    status: *const ort::sys::OrtStatus,
) -> ort::sys::OrtErrorCode {
    if status.is_null() {
        return ort::sys::OrtErrorCode::ORT_OK;
    }
    if is_bridge_status(status) {
        return ort::sys::OrtErrorCode::ORT_FAIL;
    }
    let Some(api) = api else {
        return ort::sys::OrtErrorCode::ORT_FAIL;
    };
    declared_error_code(unsafe { (api.error_code)(status) })
}

unsafe fn status_error_message(
    api: Option<&NativeStatusApi>,
    messages: &StatusMessages,
    status: *const ort::sys::OrtStatus,
) -> *const c_char {
    if status.is_null() {
        return c"ORT 未返回错误状态".as_ptr();
    }
    if is_bridge_status(status) {
        return c"OCR 原生 API 尚未发布，调用未执行".as_ptr();
    }
    let Some(api) = api else {
        return c"OCR 原生 API 尚未发布，调用未执行".as_ptr();
    };
    // 不持旁表锁调用原生函数；只有未知码或异常空消息需要旁表。
    let raw = unsafe { (api.error_code)(status) };
    let message = unsafe { (api.error_message)(status) };
    if (0..=11).contains(&raw) && !message.is_null() {
        return message;
    }
    let original = if message.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(message) })
    };
    messages.remember(status as usize, unknown_status_message(raw, original))
}

unsafe fn release_status(
    api: Option<&NativeStatusApi>,
    messages: &StatusMessages,
    status: *mut ort::sys::OrtStatus,
) {
    if status.is_null() || is_bridge_status(status) {
        return;
    }
    // 先移走旧地址的条目并解锁，防止原生释放后地址复用误清新状态。
    // 字符串在原生释放返回后才销毁，整个状态生命周期内指针稳定。
    let message = messages.detach(status as usize);
    if let Some(api) = api {
        unsafe { (api.release_status)(status) };
        #[cfg(test)]
        native_status_qa::native_release_returned(status as usize);
    }
    drop(message);
}

unsafe extern "system" fn get_error_code_safe(
    status: *const ort::sys::OrtStatus,
) -> ort::sys::OrtErrorCode {
    let code = unsafe { status_error_code(NATIVE_RUN_API.get().map(|api| &api.status), status) };
    #[cfg(test)]
    native_status_qa::published_code_returned(status as usize, code as i32);
    code
}

unsafe extern "system" fn get_error_message_safe(
    status: *const ort::sys::OrtStatus,
) -> *const c_char {
    let message = unsafe {
        status_error_message(
            NATIVE_RUN_API.get().map(|api| &api.status),
            STATUS_MESSAGES.get_or_init(StatusMessages::default),
            status,
        )
    };
    #[cfg(test)]
    native_status_qa::published_message_returned(status as usize);
    message
}

unsafe extern "system" fn release_status_safe(status: *mut ort::sys::OrtStatus) {
    unsafe {
        release_status(
            NATIVE_RUN_API.get().map(|api| &api.status),
            STATUS_MESSAGES.get_or_init(StatusMessages::default),
            status,
        )
    }
}

// 发布表安全化只影响包装层；Run 捕获一直使用原函数的整数码与原始消息，
// 避免把降为 GenericFailure 的其它错误猜成 ORT_FAIL 终止。
struct RawRunFailure {
    options_identity: usize,
    status_identity: usize,
    code: i32,
    message: Option<String>,
}

struct RunStatusCapture {
    generation: u64,
    options_identity: usize,
    observed_runs: usize,
    failure: Option<RawRunFailure>,
}

#[derive(Default)]
struct RunCaptureSlot {
    next_generation: u64,
    active: Option<RunStatusCapture>,
}

thread_local! {
    static RUN_CAPTURE: RefCell<RunCaptureSlot> = const { RefCell::new(RunCaptureSlot {
        next_generation: 0,
        active: None,
    }) };
}

struct RunOutcomeGuard {
    generation: u64,
    // 捕获与析构必须留在发起同步 Run 的线程。
    _thread_bound: PhantomData<std::rc::Rc<()>>,
}
impl RunOutcomeGuard {
    fn begin(options_identity: usize) -> Result<Self> {
        RUN_CAPTURE.with(|slot| {
            let mut slot = slot
                .try_borrow_mut()
                .map_err(|_| "OCR 原生 Run status 捕获正在使用")?;
            if slot.active.is_some() {
                return Err("OCR 原生 Run status 捕获不允许重入".into());
            }
            slot.next_generation = slot
                .next_generation
                .checked_add(1)
                .ok_or("OCR 原生 Run status 捕获身份耗尽")?;
            let generation = slot.next_generation;
            slot.active = Some(RunStatusCapture {
                generation,
                options_identity,
                observed_runs: 0,
                failure: None,
            });
            Ok(Self {
                generation,
                _thread_bound: PhantomData,
            })
        })
    }

    fn finish(self) -> Option<RawRunFailure> {
        RUN_CAPTURE.with(|slot| {
            let mut slot = slot.try_borrow_mut().ok()?;
            if slot.active.as_ref()?.generation != self.generation {
                return None;
            }
            let capture = slot.active.take()?;
            if capture.observed_runs == 1 {
                capture.failure
            } else {
                None
            }
        })
    }
}
impl Drop for RunOutcomeGuard {
    fn drop(&mut self) {
        let _ = RUN_CAPTURE.try_with(|slot| {
            if let Ok(mut slot) = slot.try_borrow_mut() {
                if slot
                    .active
                    .as_ref()
                    .is_some_and(|capture| capture.generation == self.generation)
                {
                    slot.active = None;
                }
            }
        });
    }
}

fn capture_interested(options_identity: usize) -> bool {
    RUN_CAPTURE
        .try_with(|slot| {
            slot.try_borrow().ok().is_some_and(|slot| {
                slot.active
                    .as_ref()
                    .is_some_and(|capture| capture.options_identity == options_identity)
            })
        })
        .unwrap_or(false)
}

fn record_run_status(options_identity: usize, failure: Option<RawRunFailure>) {
    let _ = RUN_CAPTURE.try_with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return;
        };
        let Some(capture) = slot.active.as_mut() else {
            return;
        };
        if capture.options_identity != options_identity {
            return;
        }
        capture.observed_runs = capture.observed_runs.saturating_add(1);
        capture.failure = failure;
    });
}

// 原始函数和库在发布 shim 前固定，status 的所有权完全留给 ort 包装。
// 这里不释放、不替换 status，也不读取任何其它 RunOptions 的结果。
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn run_with_status_capture(
    session: *mut ort::sys::OrtSession,
    run_options: *const ort::sys::OrtRunOptions,
    input_names: *const *const c_char,
    inputs: *const *const ort::sys::OrtValue,
    input_len: usize,
    output_names: *const *const c_char,
    output_names_len: usize,
    outputs: *mut *mut ort::sys::OrtValue,
) -> ort::sys::OrtStatusPtr {
    let Some(api) = NATIVE_RUN_API.get() else {
        return bridge_unavailable_status();
    };
    let status = unsafe {
        (api.run)(
            session,
            run_options,
            input_names,
            inputs,
            input_len,
            output_names,
            output_names_len,
            outputs,
        )
    };
    let options_identity = run_options as usize;
    if capture_interested(options_identity) {
        let failure = if status.0.is_null() {
            None
        } else {
            let code = unsafe { (api.status.error_code)(status.0) };
            let message = unsafe { (api.status.error_message)(status.0) };
            let message = if message.is_null() {
                None
            } else {
                unsafe { CStr::from_ptr(message) }
                    .to_str()
                    .ok()
                    .map(str::to_owned)
            };
            Some(RawRunFailure {
                options_identity,
                status_identity: status.0 as usize,
                code,
                message,
            })
        };
        record_run_status(options_identity, failure);
        #[cfg(test)]
        native_cancel_qa::native_return(session as usize, options_identity);
    }
    status
}

struct NativeRuntime {
    path: PathBuf,
}
static RUNTIME: OnceLock<NativeRuntime> = OnceLock::new();

// 调用方由 ENGINES 互斥锁串行初始化，且只接受 assets 校验后的绝对路径。
pub(super) fn initialize(path: &Path) -> Result<()> {
    let path = path.canonicalize()?;
    if let Some(runtime) = RUNTIME.get() {
        return if runtime.path == path {
            Ok(())
        } else {
            Err("当前 OCR 进程已绑定另一运行库版本，请在新会话使用新版本".into())
        };
    }
    if NATIVE_RUN_API.get().is_some() {
        return Err("OCR 原生 API 已固化但发布未完成，请在新进程重试".into());
    }
    let library = unsafe {
        Library::load_with_flags(
            &path,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    }
    .map_err(|error| format!("OCR 原生运行库加载失败（请检查 x64 VC 运行库）：{error}"))?;
    let getter = unsafe {
        library.get::<unsafe extern "C" fn() -> *const ort::sys::OrtApiBase>(b"OrtGetApiBase\0")
    }?;
    let base = unsafe { getter() };
    if base.is_null() {
        return Err("OCR 原生运行库未返回 API 基表".into());
    }
    let version = unsafe { ((*base).GetVersionString)() };
    if version.is_null() {
        return Err("OCR 原生运行库未返回版本".into());
    }
    let version = unsafe { CStr::from_ptr(version) }.to_str()?;
    if version != "1.24.4" {
        return Err(format!("OCR 运行库版本不符：需要 1.24.4，实际 {version}").into());
    }
    // 环境此时尚未创建；只跳过其中无用的 SetupAPI 设备枚举，失败时保持原样。
    let full_scan =
        cfg!(test) && std::env::var("RUSTSHOT_OCR_DEVICE_SCAN").as_deref() == Ok("full");
    if !full_scan {
        let mut module = HMODULE::default();
        let found = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(*getter as *const u16),
                &mut module,
            )
        };
        if found.is_ok() && unsafe { super::discovery::skip_setupapi_scan(module) } {
            DEVICE_SCAN_SKIPPED.store(true, Ordering::Release);
        }
    }
    let api = unsafe { ((*base).GetApi)(ort::sys::ORT_API_VERSION) };
    if api.is_null() {
        return Err("OCR 原生运行库不支持固定 API24".into());
    }
    // alternative-backend 禁止 ort 隐式按 PATH 搜索 DLL。只发布核验后的表，
    // 并永久持有其所属库；空 API 不会进入绑定中的断言。
    let mut api = unsafe { std::ptr::read(api) };
    NATIVE_RUN_API
        .set(NativeRunApi {
            run: api.Run,
            status: NativeStatusApi {
                // 固定 Windows C ABI 的 OrtErrorCode 为 32 位；rc.13 只声明0..11。
                // 按整数读取原函数，不构造12／13／14或未知 Rust 枚举值。
                error_code: unsafe {
                    std::mem::transmute::<
                        unsafe extern "system" fn(
                            *const ort::sys::OrtStatus,
                        )
                            -> ort::sys::OrtErrorCode,
                        unsafe extern "system" fn(*const ort::sys::OrtStatus) -> i32,
                    >(api.GetErrorCode)
                },
                error_message: api.GetErrorMessage,
                release_status: api.ReleaseStatus,
            },
            _library: library,
        })
        .map_err(|_| "OCR 原生 Run API 初始化冲突")?;
    api.Run = run_with_status_capture;
    api.GetErrorCode = get_error_code_safe;
    api.GetErrorMessage = get_error_message_safe;
    api.ReleaseStatus = release_status_safe;
    if !ort::set_api(api) {
        return Err("OCR 推理 API 已被其它后端初始化".into());
    }
    RUNTIME
        .set(NativeRuntime { path })
        .map_err(|_| "OCR 运行库初始化冲突")?;
    ort::init().with_name("RustShot OCR CPU").commit();
    Ok(())
}

static DEVICE_SCAN_SKIPPED: AtomicBool = AtomicBool::new(false);

/// 本进程的运行库是否已跳过 SetupAPI 设备枚举（供诊断记录）。
pub(super) fn device_scan_skipped() -> bool {
    DEVICE_SCAN_SKIPPED.load(Ordering::Acquire)
}

/// 首个 Session 才会实体化 ORT 全局环境；提前单独调用只为分段计时，
/// 后续 Session 复用同一环境，构造顺序与配置不变。
pub(super) fn environment() -> Result<()> {
    if RUNTIME.get().is_none() {
        return Err("OCR 运行库尚未初始化".into());
    }
    ort::environment::Environment::current()?;
    Ok(())
}

pub(super) struct CpuSession {
    session: Session,
}
impl CpuSession {
    pub(super) fn new(model: &Path, intra_threads: usize) -> Result<Self> {
        if !matches!(intra_threads, 1 | 2 | 4) {
            return Err("OCR CPU 线程数仅支持 1、2 或 4".into());
        }
        if RUNTIME.get().is_none() {
            return Err("OCR 运行库尚未初始化".into());
        }
        let session = Session::builder()?
            .with_intra_threads(intra_threads)?
            .with_inter_threads(1)?
            .with_parallel_execution(false)?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .commit_from_file(model)?;
        Ok(Self { session })
    }

    pub(super) fn custom_metadata(&self, key: &str) -> Result<Option<String>> {
        if key.is_empty() || key.len() > 1024 || key.contains('\0') {
            return Err("模型元数据键无效".into());
        }
        Ok(self.session.metadata()?.custom(key))
    }
}

struct TaskRunControl {
    options: Arc<RunOptions>,
    // 只保护一次 terminate 的接受资格，不覆盖 Run，也不获取 ENGINES 锁。
    accepted: Mutex<bool>,
}
impl RunCancellation for TaskRunControl {
    fn terminate(&self) -> Result<()> {
        let mut accepted = self.accepted.lock();
        if !*accepted {
            self.options.terminate()?;
            *accepted = true;
        }
        Ok(())
    }
}

/// 每请求一个上下文；det／cls／rec 共用选项，下一请求重新构造，不清终止位。
pub(super) struct TaskRunContext {
    _registration: RunRegistration,
    token: TaskToken,
    options: Arc<RunOptions>,
    control: Arc<TaskRunControl>,
}
impl TaskRunContext {
    pub(super) fn new(token: &TaskToken) -> Result<Self> {
        token.checkpoint()?;
        if RUNTIME.get().is_none() {
            return Err("OCR 运行库尚未初始化".into());
        }
        let options = RunOptions::new()?;
        #[cfg(test)]
        let options = native_cancel_qa::configure_options(options)?;
        let options = Arc::new(options);
        let control = Arc::new(TaskRunControl {
            options: Arc::clone(&options),
            accepted: Mutex::new(false),
        });
        let registration = token.register(control.clone())?;
        token.checkpoint()?;
        Ok(Self {
            _registration: registration,
            token: token.clone(),
            options,
            control,
        })
    }

    pub(super) fn runner<'a>(&'a self, session: &'a mut CpuSession) -> TaskSession<'a> {
        TaskSession {
            context: self,
            session,
        }
    }

    fn classify_run_error(
        &self,
        error: ort::Error,
        raw: Option<RawRunFailure>,
    ) -> Box<dyn std::error::Error> {
        let task_cancelled = self.token.is_cancelled();
        // 锁等待只与短 terminate 回调相遇；同步成功之前不能冒称被本选项取消。
        let termination_accepted = *self.control.accepted.lock();
        if matches_task_termination(
            task_cancelled,
            termination_accepted,
            self.options.ptr() as usize,
            raw.as_ref(),
            error.code(),
            error.message(),
        ) {
            Box::new(OcrCancelled)
        } else {
            Box::new(error)
        }
    }
}

pub(super) struct TaskSession<'a> {
    context: &'a TaskRunContext,
    session: &'a mut CpuSession,
}

const TERMINATE_MESSAGE: &str = "Exiting due to terminate flag being set to true.";

fn matches_task_termination(
    task_cancelled: bool,
    termination_accepted: bool,
    options_identity: usize,
    raw: Option<&RawRunFailure>,
    code: ort::ErrorCode,
    message: &str,
) -> bool {
    task_cancelled
        && termination_accepted
        && options_identity != 0
        && code == ort::ErrorCode::GenericFailure
        && message == TERMINATE_MESSAGE
        && raw.is_some_and(|raw| {
            raw.options_identity == options_identity
                && raw.status_identity != 0
                && raw.code == ort::sys::OrtErrorCode::ORT_FAIL as i32
                && raw.message.as_deref() == Some(TERMINATE_MESSAGE)
        })
}

fn tensor_length(shape: &[usize]) -> Result<usize> {
    if shape.is_empty() || shape.contains(&0) {
        return Err("OCR tensor 形状无效".into());
    }
    let count = shape
        .iter()
        .try_fold(1usize, |total, side| total.checked_mul(*side))
        .ok_or("OCR tensor 尺寸溢出")?;
    if count > 64 * 1024 * 1024 {
        return Err("OCR tensor 超过工作区预算".into());
    }
    Ok(count)
}
impl ModelRunner for CpuSession {
    fn run(&mut self, input: Tensor) -> Result<Tensor> {
        if tensor_length(&input.shape)? != input.values.len() {
            return Err("OCR tensor 形状与数据长度不符".into());
        }
        let tensor = ort::value::Tensor::from_array((input.shape, input.values))?;
        let outputs = self.session.run(ort::inputs![tensor])?;
        output_tensor(&outputs)
    }
}

impl ModelRunner for TaskSession<'_> {
    fn run(&mut self, input: Tensor) -> Result<Tensor> {
        self.run_extracted(input, output_tensor)
    }
}

impl TaskSession<'_> {
    /// 表格结构使用多个具名输出；普通 OCR 继续只读取 fetch_name_0。
    pub(super) fn run_named(&mut self, input: Tensor) -> Result<Vec<(String, Tensor)>> {
        self.run_extracted(input, named_output_tensors)
    }

    fn run_extracted<T>(
        &mut self,
        input: Tensor,
        extract: impl FnOnce(&SessionOutputs<'_>) -> Result<T>,
    ) -> Result<T> {
        self.context.token.checkpoint()?;
        if tensor_length(&input.shape)? != input.values.len() {
            return Err("OCR tensor 形状与数据长度不符".into());
        }
        let tensor = ort::value::Tensor::from_array((input.shape, input.values))?;
        self.context.token.checkpoint()?;
        let outcome = RunOutcomeGuard::begin(self.context.options.ptr() as usize)?;
        // tensor 由 inputs 持有至同步调用返回；session 与 options 均受本借用保护。
        let result = self
            .session
            .session
            .run_with_options(ort::inputs![tensor], self.context.options.as_ref());
        let raw = outcome.finish();
        match result {
            Ok(outputs) => {
                // 通用提取错误先保留；det／cls／CTC 的语义验证由管线完成，
                // 再在其成功边界检查取消，避免覆盖同时发生的真实输出错误。
                extract(&outputs)
            }
            Err(error) => Err(self.context.classify_run_error(error, raw)),
        }
    }
}

fn named_output_tensors(outputs: &SessionOutputs<'_>) -> Result<Vec<(String, Tensor)>> {
    if outputs.len() == 0 || outputs.len() > 16 {
        return Err("表格模型输出数量无效".into());
    }
    let mut tensors = Vec::with_capacity(outputs.len());
    let mut total = 0usize;
    for (name, output) in outputs {
        if name.is_empty()
            || name.len() > 1024
            || tensors.iter().any(|(existing, _)| existing == name)
        {
            return Err("表格模型输出名称无效".into());
        }
        let (shape, values) = output.try_extract_tensor::<f32>()?;
        if shape.len() > 8 {
            return Err("表格模型输出维数超过工作区预算".into());
        }
        let shape: Vec<usize> = shape
            .iter()
            .map(|side| usize::try_from(*side))
            .collect::<std::result::Result<_, _>>()?;
        let count = tensor_length(&shape)?;
        if count != values.len() {
            return Err("表格模型输出 tensor 无效".into());
        }
        total = total.checked_add(count).ok_or("表格模型输出尺寸溢出")?;
        if total > 64 * 1024 * 1024 {
            return Err("表格模型输出总量超过工作区预算".into());
        }
        tensors.push((
            name.to_owned(),
            Tensor {
                shape,
                values: values.to_vec(),
            },
        ));
    }
    Ok(tensors)
}

fn output_tensor(outputs: &SessionOutputs<'_>) -> Result<Tensor> {
    let output = outputs
        .get("fetch_name_0")
        .ok_or("OCR 模型缺少固定输出 fetch_name_0")?;
    let (shape, values) = output.try_extract_tensor::<f32>()?;
    let shape: Vec<usize> = shape
        .iter()
        .map(|side| usize::try_from(*side))
        .collect::<std::result::Result<_, _>>()?;
    if tensor_length(&shape)? != values.len() {
        return Err("OCR 模型输出 tensor 无效".into());
    }
    Ok(Tensor {
        shape,
        values: values.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_thread_choices_are_rejected_before_runtime_or_model_access() {
        for threads in [0, 3, 5, 8, usize::MAX] {
            let error = CpuSession::new(Path::new("未加载的模型.onnx"), threads)
                .err()
                .expect("非法线程档位必须失败");
            assert_eq!(error.to_string(), "OCR CPU 线程数仅支持 1、2 或 4");
        }
    }

    #[test]
    fn tensor_budget_rejects_zero_overflow_and_oversized_shapes() {
        assert!(tensor_length(&[]).is_err());
        assert!(tensor_length(&[1, 0]).is_err());
        assert!(tensor_length(&[usize::MAX, 2]).is_err());
        assert!(tensor_length(&[1, 3, 100_000, 100_000]).is_err());
        assert_eq!(tensor_length(&[1, 3, 48, 320]).unwrap(), 46_080);
    }
}

#[cfg(test)]
#[path = "runtime_cancel_tests.rs"]
mod cancellation_tests;

#[cfg(test)]
#[path = "runtime_native_cancel_qa.rs"]
mod native_cancel_qa;

#[cfg(test)]
#[path = "runtime_native_status_tests.rs"]
mod native_status_tests;

#[cfg(test)]
#[path = "runtime_native_status_qa.rs"]
mod native_status_qa;
