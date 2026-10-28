//! 仅 cfg(test) 的真实 API24 状态转换／释放夹具；不创建模型 Session 或访问前台。
use super::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
};

#[derive(Clone, Copy, Debug, Default)]
struct Observation {
    identity: usize,
    code_calls: u32,
    message_calls: u32,
    release_returns: u32,
    safe_code: Option<i32>,
    sequence: u32,
    code_order: u32,
    message_order: u32,
    release_order: u32,
}

thread_local! {
    static STATUS_PROBE: RefCell<Option<Observation>> = const { RefCell::new(None) };
}

// 仅同步包装线程的一条 status 身份；钩子不分配、不写文件、不 panic／unwind。
fn observe(identity: usize, update: impl FnOnce(&mut Observation)) {
    let _ = STATUS_PROBE.try_with(|slot| {
        let Ok(mut slot) = slot.try_borrow_mut() else {
            return;
        };
        let Some(observation) = slot.as_mut() else {
            return;
        };
        if observation.identity == identity {
            observation.sequence = observation.sequence.saturating_add(1);
            update(observation);
        }
    });
}

pub(super) fn published_code_returned(identity: usize, code: i32) {
    observe(identity, |observation| {
        observation.code_calls = observation.code_calls.saturating_add(1);
        observation.safe_code = Some(code);
        observation.code_order = observation.sequence;
    });
}

pub(super) fn published_message_returned(identity: usize) {
    observe(identity, |observation| {
        observation.message_calls = observation.message_calls.saturating_add(1);
        observation.message_order = observation.sequence;
    });
}

// 必须放在 release_status 中原生 release_status 回调正常返回之后，
// 不放在 ReleaseStatus shim 入口；计数证明原生释放函数确已被调用并返回。
pub(super) fn native_release_returned(identity: usize) {
    observe(identity, |observation| {
        observation.release_returns = observation.release_returns.saturating_add(1);
        observation.release_order = observation.sequence;
    });
}

struct ProbeScope {
    identity: usize,
    _thread_bound: PhantomData<std::rc::Rc<()>>,
}

impl ProbeScope {
    fn begin(identity: usize) -> Result<Self> {
        if identity == 0 || is_bridge_status(identity as *const ort::sys::OrtStatus) {
            return Err("真实状态观察不接受 null 或桥接标记".into());
        }
        STATUS_PROBE.with(|slot| -> Result<()> {
            let mut slot = slot.try_borrow_mut()?;
            if slot.is_some() {
                return Err("真实状态观察不允许重入".into());
            }
            *slot = Some(Observation {
                identity,
                ..Observation::default()
            });
            Ok(())
        })?;
        Ok(Self {
            identity,
            _thread_bound: PhantomData,
        })
    }

    fn snapshot(&self) -> Result<Observation> {
        STATUS_PROBE.with(|slot| {
            let slot = slot.try_borrow()?;
            slot.as_ref()
                .filter(|observation| observation.identity == self.identity)
                .copied()
                .ok_or_else(|| "真实状态观察身份已丢失".into())
        })
    }
}

impl Drop for ProbeScope {
    fn drop(&mut self) {
        let _ = STATUS_PROBE.try_with(|slot| {
            if let Ok(mut slot) = slot.try_borrow_mut() {
                if slot
                    .as_ref()
                    .is_some_and(|observation| observation.identity == self.identity)
                {
                    *slot = None;
                }
            }
        });
    }
}

// 原 status 从返回的第一刻就受 RAII 保护；transfer 后完全交给 ort::Error。
struct PendingStatus(*mut ort::sys::OrtStatus);

impl PendingStatus {
    fn new(status: ort::sys::OrtStatusPtr) -> Self {
        Self(status.0)
    }

    fn transfer(&mut self) -> ort::sys::OrtStatusPtr {
        ort::sys::OrtStatusPtr(std::mem::replace(&mut self.0, std::ptr::null_mut()))
    }
}

impl Drop for PendingStatus {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (ort::api().ReleaseStatus)(self.0) };
        }
    }
}

// 成功输出也受 RAII 保护；即使参数错误意外写出了非空对象，仍正常释放。
struct MemoryInfoOwner(*mut ort::sys::OrtMemoryInfo);

impl MemoryInfoOwner {
    fn empty() -> Self {
        Self(std::ptr::null_mut())
    }
}

impl Drop for MemoryInfoOwner {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { (ort::api().ReleaseMemoryInfo)(self.0) };
        }
    }
}

struct RecordSink(File);

impl RecordSink {
    fn open() -> Result<Self> {
        let path = PathBuf::from(std::env::var("RUSTSHOT_OCR_NATIVE_STATUS_QA_RECORD")?);
        let allowed = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa/ppocr-native-status-20261010")
            .canonicalize()?;
        let parent = path
            .parent()
            .ok_or("状态 QA 回执没有父目录")?
            .canonicalize()?;
        if !path.is_absolute() || !parent.starts_with(&allowed) {
            return Err(
                "状态 QA 回执只能写入 target/qa/ppocr-native-status-20261010 子目录".into(),
            );
        }
        Ok(Self(
            OpenOptions::new().write(true).create_new(true).open(path)?,
        ))
    }

    fn append(&mut self, event: Value) -> Result<()> {
        writeln!(self.0, "{}", serde_json::to_string(&event)?)?;
        self.0.flush()?;
        Ok(())
    }
}

fn single_component(value: &str) -> Result<&str> {
    if value.is_empty() || value == "." || value == ".." || value.contains(['/', '\\', ':']) {
        Err("状态 QA 固定资产名不是单一组件".into())
    } else {
        Ok(value)
    }
}

fn verify_file(path: &Path, expected: &Value) -> Result<Value> {
    let actual = path.canonicalize()?;
    let bytes = std::fs::read(&actual)?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    if Some(bytes.len() as u64) != expected["bytes"].as_u64()
        || Some(sha256.as_str()) != expected["sha256"].as_str()
    {
        return Err(format!("状态 QA 固定缓存 bytes/SHA 不符：{}", actual.display()).into());
    }
    Ok(json!({"path":actual, "bytes":bytes.len(), "sha256":sha256}))
}

fn cached_assets() -> Result<(PathBuf, Vec<Value>)> {
    // 与既有暖 Run 夹具相同六件，只读核验；不调用 assets::ensure* 或联网。
    let manifest: Value = serde_json::from_str(include_str!("../../dependencies/ppocr-cpu.json"))?;
    if manifest["runtime_api"] != 24 || ort::sys::ORT_API_VERSION != 24 {
        return Err("状态 QA 仅接受固定 API24".into());
    }
    let cache = PathBuf::from(std::env::var("RUSTSHOT_OCR_NATIVE_STATUS_QA_CACHE")?);
    if !cache.is_absolute() {
        return Err("状态 QA 缓存路径必须为绝对路径".into());
    }
    let cache = cache.canonicalize()?;
    if cache.file_name().and_then(|name| name.to_str()) != manifest["cache_revision"].as_str() {
        return Err("状态 QA 缓存目录与固定 cache_revision 不一致".into());
    }
    let runtime_dir = cache.join(single_component(
        manifest["runtime"]["directory"]
            .as_str()
            .ok_or("状态 QA 运行库目录缺失")?,
    )?);
    let model_dir = cache.join(single_component(
        manifest["models"]["directory"]
            .as_str()
            .ok_or("状态 QA 模型目录缺失")?,
    )?);
    let runtime_asset = manifest["runtime"]["files"]
        .as_array()
        .ok_or("状态 QA 运行库固定文件表缺失")?
        .iter()
        .find(|asset| asset["name"] == "onnxruntime.dll")
        .ok_or("状态 QA 固定 DLL 缺失")?;
    let runtime = runtime_dir.join("onnxruntime.dll").canonicalize()?;
    let mut identities = vec![verify_file(&runtime, runtime_asset)?];
    for key in [
        "license",
        "detection",
        "classification",
        "recognition",
        "dictionary",
    ] {
        let asset = &manifest["models"][key];
        let name = single_component(asset["name"].as_str().ok_or("状态 QA 资产名缺失")?)?;
        identities.push(verify_file(&model_dir.join(name), asset)?);
    }
    Ok((runtime, identities))
}

fn consume_error(
    label: &str,
    mut status: PendingStatus,
    expected_raw_code: i32,
    expected_code: ort::ErrorCode,
    expected_message: Option<&str>,
    sink: &mut RecordSink,
) -> Result<Value> {
    if status.0.is_null() {
        return Err(format!("{label} 没有产生真实非空 status").into());
    }
    let native = NATIVE_RUN_API.get().ok_or("状态 QA 原生 API 尚未固化")?;
    let raw_code = unsafe { (native.status.error_code)(status.0) };
    let raw_message_pointer = unsafe { (native.status.error_message)(status.0) };
    if raw_message_pointer.is_null() {
        return Err(format!("{label} 原生消息为空").into());
    }
    let raw_message = unsafe { CStr::from_ptr(raw_message_pointer) }
        .to_str()?
        .to_owned();
    let identity = status.0 as usize;
    let scope = ProbeScope::begin(identity)?;
    // 原 API 整数／消息快照已读完；以下真实包装调用只能经发布的三个 shim。
    let error = unsafe { ort::Error::result_from_status(status.transfer()) }
        .err()
        .ok_or("非空原生状态被包装成了成功")?;
    let wrapper_code = error.code();
    let wrapper_message = error.message().to_owned();
    let live = scope.snapshot()?;
    drop(error);
    let released = scope.snapshot()?;
    drop(scope);
    let cache_cleared = STATUS_MESSAGES
        .get()
        .is_none_or(|messages| !messages.messages.lock().contains_key(&identity));
    let event = json!({
        "event":"native_error_converted_and_released", "label":label,
        "status_identity":format!("0x{identity:016X}"), "raw_code":raw_code,
        "raw_message":raw_message, "wrapper_code":format!("{wrapper_code:?}"),
        "wrapper_message":wrapper_message, "published_code_calls":released.code_calls,
        "published_message_calls":released.message_calls,
        "native_release_returns":released.release_returns,
        "release_returns_while_error_alive":live.release_returns,
        "callback_order":[released.code_order,released.message_order,released.release_order],
        "status_message_cache_cleared":cache_cleared,
    });
    sink.append(event.clone())?;
    let message_matches = if (1..=11).contains(&raw_code) {
        wrapper_message == raw_message
    } else {
        let name = match raw_code {
            12 => "ORT_MODEL_LOAD_CANCELED",
            13 => "ORT_MODEL_REQUIRES_COMPILATION",
            14 => "ORT_NOT_FOUND",
            _ => return Err("本真实夹具不接受未来未知整数".into()),
        };
        wrapper_message.starts_with(&format!("ORT 原始错误码 {raw_code}（{name}）："))
            && wrapper_message.ends_with(&raw_message)
    };
    let expected_safe_code = if raw_code <= 11 { raw_code } else { 1 };
    if raw_code != expected_raw_code
        || !(1..=14).contains(&raw_code)
        || released.safe_code != Some(expected_safe_code)
        || wrapper_code != expected_code
        || !message_matches
        || expected_message.is_some_and(|message| message != raw_message)
        || live.release_returns != 0
        || released.code_calls != 1
        || released.message_calls != 1
        || released.release_returns != 1
        || [
            released.code_order,
            released.message_order,
            released.release_order,
        ] != [1, 2, 3]
        || !cache_cleared
    {
        return Err(format!("{label} 的真实转换／消息／一次释放合同不符").into());
    }
    Ok(event)
}

fn successful_cpu_memory_info(label: &str, sink: &mut RecordSink) -> Result<()> {
    let api = ort::api();
    let mut memory = MemoryInfoOwner::empty();
    let mut status = PendingStatus::new(unsafe {
        (api.CreateCpuMemoryInfo)(
            ort::sys::OrtAllocatorType::OrtArenaAllocator,
            ort::sys::OrtMemType::OrtMemTypeDefault,
            &mut memory.0,
        )
    });
    unsafe { ort::Error::result_from_status(status.transfer()) }?;
    if memory.0.is_null() {
        return Err("状态 QA 正常 CPU MemoryInfo 输出为空".into());
    }
    let mut name = std::ptr::null();
    let mut status = PendingStatus::new(unsafe { (api.MemoryInfoGetName)(memory.0, &mut name) });
    unsafe { ort::Error::result_from_status(status.transfer()) }?;
    if name.is_null() || unsafe { CStr::from_ptr(name) }.to_bytes() != b"Cpu" {
        return Err("状态 QA 正常 CPU MemoryInfo 名称不符".into());
    }
    // MemoryInfo 只含设备描述，不分配 GPU 设备、模型线程池或截图像素。
    drop(memory);
    sink.append(json!({"event":"followup_cpu_api_success", "label":label,
        "memory_info_name":"Cpu", "memory_info_released":true}))?;
    Ok(())
}

fn execute(sink: &mut RecordSink) -> Result<Value> {
    let (runtime, before) = cached_assets()?;
    initialize(&runtime)?;
    let api = ort::api();
    sink.append(json!({"event":"preflight", "cache":before,
        "runtime_api":24, "runtime_version":"1.24.4", "model_sessions_created":0,
        "scope":"真实参数错误、1..11及官方12..14原生构造状态，经真实ort::Error转换／释放；不资格化未来未知整数或Run"}))?;
    successful_cpu_memory_info("before_errors", sink)?;

    // 指针、输出槽与枚举参数均有效；仅设备名非法，原生显式返回 INVALID_ARGUMENT。
    // 此已声明码先证明三个发布shim确实被真实ort::Error调用；失败立即停止，
    // 不进入12..14。观察合同不依赖跨codegen-unit的函数地址相等假设。
    let mut memory = MemoryInfoOwner::empty();
    let status = PendingStatus::new(unsafe {
        (api.CreateMemoryInfo)(
            c"RustShot.native.status.qa.invalid-device".as_ptr(),
            ort::sys::OrtAllocatorType::OrtArenaAllocator,
            0,
            ort::sys::OrtMemType::OrtMemTypeDefault,
            &mut memory.0,
        )
    });
    consume_error(
        "actual_invalid_device_parameter",
        status,
        2,
        ort::ErrorCode::InvalidArgument,
        Some("Specified device is not supported. Try CreateMemoryInfo_V2."),
        sink,
    )?;
    let output_stayed_null = memory.0.is_null();
    drop(memory);
    if !output_stayed_null {
        return Err("非法设备参数意外写出了 MemoryInfo 对象（已释放）".into());
    }
    successful_cpu_memory_info("after_actual_parameter_error", sink)?;

    // 这11项使用真正 DLL CreateStatus 分配，资格称“原生构造状态矩阵”，
    // 不称其它11种真实业务失败，更不向 CreateStatus 传未知 enum 判别值。
    use ort::sys::OrtErrorCode::*;
    let known = [
        (ORT_FAIL, ort::ErrorCode::GenericFailure),
        (ORT_INVALID_ARGUMENT, ort::ErrorCode::InvalidArgument),
        (ORT_NO_SUCHFILE, ort::ErrorCode::NoSuchFile),
        (ORT_NO_MODEL, ort::ErrorCode::NoModel),
        (ORT_ENGINE_ERROR, ort::ErrorCode::EngineError),
        (ORT_RUNTIME_EXCEPTION, ort::ErrorCode::RuntimeException),
        (ORT_INVALID_PROTOBUF, ort::ErrorCode::InvalidProtobuf),
        (ORT_MODEL_LOADED, ort::ErrorCode::ModelLoaded),
        (ORT_NOT_IMPLEMENTED, ort::ErrorCode::NotImplemented),
        (ORT_INVALID_GRAPH, ort::ErrorCode::InvalidGraph),
        (ORT_EP_FAIL, ort::ErrorCode::ExecutionProviderFailure),
    ];
    for (code, expected) in known {
        let label = format!("native_create_status_code_{}", code as i32);
        let text = format!("RustShot 状态 QA；原生构造码 {}", code as i32);
        let message = CString::new(text.as_bytes())?;
        let status = PendingStatus::new(unsafe { (api.CreateStatus)(code, message.as_ptr()) });
        consume_error(&label, status, code as i32, expected, Some(&text), sink)?;
        successful_cpu_memory_info(&label, sink)?;
    }
    // 官方1.24.4 enum已声明12..14，CreateStatus直接保存非零码并复制消息。
    // 同Windows ABI以i32传入，避免在旧ort-sys枚举中构造未声明判别值。
    let create_status_integer = unsafe {
        std::mem::transmute::<
            unsafe extern "system" fn(
                ort::sys::OrtErrorCode,
                *const c_char,
            ) -> ort::sys::OrtStatusPtr,
            unsafe extern "system" fn(i32, *const c_char) -> ort::sys::OrtStatusPtr,
        >(api.CreateStatus)
    };
    for raw in [12, 13, 14] {
        let label = format!("native_create_status_api24_new_code_{raw}");
        let text = format!("RustShot 官方API24新状态 QA；原生构造码 {raw}");
        let message = CString::new(text.as_bytes())?;
        let status = PendingStatus::new(unsafe { create_status_integer(raw, message.as_ptr()) });
        consume_error(
            &label,
            status,
            raw,
            ort::ErrorCode::GenericFailure,
            Some(&text),
            sink,
        )?;
        successful_cpu_memory_info(&label, sink)?;
    }
    let (after_runtime, after) = cached_assets()?;
    if runtime != after_runtime || before != after {
        return Err("状态 QA 前后六件固定资产身份改变".into());
    }
    Ok(json!({"event":"qualification", "ok":true,
        "actual_invalid_argument":true, "actual_parameter_error_code":2,
        "native_constructed_declared_code_matrix":[1,2,3,4,5,6,7,8,9,10,11],
        "native_constructed_api24_new_code_matrix":[12,13,14],
        "api24_new_codes_ort_error_conversion":true,
        "session_or_run_produced_api24_new_codes":false,
        "real_ort_error_conversion":true, "native_release_returns_per_error":1,
        "error_cases":15, "successful_followup_cpu_api_cases":16,
        "six_fixed_assets_unchanged":true, "real_unknown_native_codes":false,
        "unknown_codes_require_separate_19_models":true, "unknown_models_executed_by_this_test":false,
        "model_sessions_created":0, "model_run":false, "warm_run_cancel_recovery":false,
        "session_settings_changed":false, "network":false, "gui":false,
        "desktop_or_clipboard_read":false, "capture_latency":false}))
}

#[test]
#[ignore = "仅后台独占QA进程；固定本机缓存；不下载、不加载模型、不访问桌面"]
fn real_native_status_conversion_release_and_followup_cpu_api() -> Result<()> {
    let mut sink = RecordSink::open()?;
    match execute(&mut sink) {
        Ok(qualification) => sink.append(qualification),
        Err(error) => {
            sink.append(json!({"event":"qualification", "ok":false,
                "error":error.to_string(), "real_unknown_native_codes":false}))?;
            Err(error)
        }
    }
}
