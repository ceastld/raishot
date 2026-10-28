//! 仅 cfg(test) 注册的实际 CPU Run 取消夹具；不能作为产品统计或前台验证入口。
use super::*;
use crate::recognition::task::{is_cancelled, CancelOwner};
use ort::logging::LogLevel;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

const INPUT_WIDTH: usize = 4096;
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_RUN: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static QA_TAG: RefCell<Option<String>> = const { RefCell::new(None) };
    static QA_PROBE: RefCell<Option<Arc<Probe>>> = const { RefCell::new(None) };
}

// 唯一选项配置钩子，放在 TaskRunContext::new 的 Arc 构造之前。非本夹具
// 任务完全保留原选项；只修改测试的 tag，不修改终止位或产品错误分类。
pub(super) fn configure_options(options: RunOptions) -> Result<RunOptions> {
    let tag = QA_TAG.try_with(|tag| tag.try_borrow().ok().and_then(|tag| tag.clone()))?;
    match tag {
        Some(tag) => Ok(options.with_tag(tag)?),
        None => Ok(options),
    }
}

// 唯一返回观察钩子，放在真实 shim 的 record_run_status 之后、原 status
// 交还 ort 包装之前。复制值，不释放 status，不改变 Run 或错误对象。
pub(super) fn native_return(session_identity: usize, options_identity: usize) {
    let probe = QA_PROBE
        .try_with(|probe| probe.try_borrow().ok().and_then(|probe| probe.clone()))
        .ok()
        .flatten();
    let Some(probe) = probe else { return };
    let captured = RUN_CAPTURE
        .try_with(|slot| {
            let slot = slot.try_borrow().ok()?;
            let capture = slot.active.as_ref()?;
            Some(ReturnEvidence {
                session_identity,
                options_identity,
                generation: capture.generation,
                observed_runs: capture.observed_runs,
                status_identity: capture
                    .failure
                    .as_ref()
                    .map(|failure| failure.status_identity),
                raw_code: capture.failure.as_ref().map(|failure| failure.code),
                raw_message: capture
                    .failure
                    .as_ref()
                    .and_then(|failure| failure.message.clone()),
                failure_options_identity: capture
                    .failure
                    .as_ref()
                    .map(|failure| failure.options_identity),
                tick_ns: probe.sink.start.elapsed().as_nanos(),
            })
        })
        .ok()
        .flatten();
    let Some(captured) = captured else {
        probe.fail("原生返回时没有本线程 RunOutcomeGuard");
        return;
    };
    probe.sink.append(json!({
        "event": "native_run_return", "run_id": probe.run_id, "phase": probe.phase,
        "session_identity": identity(captured.session_identity),
        "options_identity": identity(captured.options_identity),
        "generation": captured.generation, "observed_runs": captured.observed_runs,
        "status_identity": captured.status_identity.map(identity), "raw_code": captured.raw_code,
        "raw_message": captured.raw_message, "tick_ns": captured.tick_ns.to_string(),
    }));
    let mut state = probe.state.lock();
    if state.returned.is_some() {
        state.errors.push("本次 Run 产生多次原生返回".into());
    }
    state.returned = Some(captured);
}

struct RecordSink {
    file: Mutex<File>,
    errors: Mutex<Vec<String>>,
    start: Instant,
}

impl RecordSink {
    fn open() -> Result<Arc<Self>> {
        let path = PathBuf::from(std::env::var("RUSTSHOT_OCR_NATIVE_CANCEL_QA_RECORD")?);
        let allowed = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa/ocr-native-cancel-20261010")
            .canonicalize()?;
        let parent = path.parent().ok_or("QA 回执没有父目录")?.canonicalize()?;
        if !path.is_absolute() || !parent.starts_with(&allowed) {
            return Err("QA 回执只能写入独占的 target/qa/ocr-native-cancel-20261010 子目录".into());
        }
        Ok(Arc::new(Self {
            file: Mutex::new(OpenOptions::new().write(true).create_new(true).open(path)?),
            errors: Mutex::new(Vec::new()),
            start: Instant::now(),
        }))
    }

    // 原生日志回调不能 panic/unwind；记录错误，测试返回后统一失败。
    fn append(&self, event: Value) {
        let result = (|| -> Result<()> {
            let line = serde_json::to_string(&event)?;
            let mut file = self.file.lock();
            writeln!(file, "{line}")?;
            file.flush()?;
            Ok(())
        })();
        if let Err(error) = result {
            self.errors.lock().push(error.to_string());
        }
    }
}

#[derive(Clone)]
struct EntryEvidence {
    generation: u64,
    options_identity: usize,
    tick_ns: u128,
}

#[derive(Clone)]
struct ReturnEvidence {
    session_identity: usize,
    options_identity: usize,
    generation: u64,
    observed_runs: usize,
    status_identity: Option<usize>,
    raw_code: Option<i32>,
    raw_message: Option<String>,
    failure_options_identity: Option<usize>,
    tick_ns: u128,
}

struct CancelReceipt {
    accepted: bool,
    error: Option<String>,
    generation: Option<u64>,
    options_identity: Option<usize>,
    tick_ns: u128,
}

struct CancelGate {
    entered: Sender<EntryEvidence>,
    accepted: Receiver<CancelReceipt>,
}

#[derive(Default)]
struct ProbeState {
    entered: Option<EntryEvidence>,
    returned: Option<ReturnEvidence>,
    cancel_tick_ns: Option<u128>,
    errors: Vec<String>,
}

struct Probe {
    run_id: u64,
    phase: &'static str,
    tag: String,
    log_id: String,
    session_identity: usize,
    options_identity: usize,
    thread_id: thread::ThreadId,
    state: Mutex<ProbeState>,
    gate: Mutex<Option<CancelGate>>,
    sink: Arc<RecordSink>,
}

impl Probe {
    fn fail(&self, message: &str) {
        self.state.lock().errors.push(message.to_owned());
        self.sink
            .append(json!({"event":"probe_error", "run_id":self.run_id, "message":message}));
    }
}

fn identity(value: usize) -> String {
    format!("0x{value:016X}")
}

fn native_entry_matches(level: LogLevel, location: &str, message: &str, tag: &str) -> bool {
    let function = location.split_whitespace().last().unwrap_or("");
    level == LogLevel::Info
        && location.contains("inference_session.cc:")
        && (function == "Run" || function.ends_with("::Run"))
        && message == format!("Running with tag: {tag}")
}

fn native_log(level: LogLevel, _category: &str, log_id: &str, location: &str, message: &str) {
    let probe = QA_PROBE
        .try_with(|probe| probe.try_borrow().ok().and_then(|probe| probe.clone()))
        .ok()
        .flatten();
    let Some(probe) = probe else { return };
    if !native_entry_matches(level, location, message, &probe.tag) {
        return;
    }
    if log_id != probe.log_id || thread::current().id() != probe.thread_id {
        probe.fail("原生进入信号不属于指定 session 日志或 Run 线程");
        return;
    }
    let captured = RUN_CAPTURE
        .try_with(|slot| {
            let slot = slot.try_borrow().ok()?;
            let active = slot.active.as_ref()?;
            (active.options_identity == probe.options_identity && active.observed_runs == 0)
                .then_some(EntryEvidence {
                    generation: active.generation,
                    options_identity: active.options_identity,
                    tick_ns: probe.sink.start.elapsed().as_nanos(),
                })
        })
        .ok()
        .flatten();
    let Some(captured) = captured else {
        probe.fail("原生进入信号缺少尚未返回且选项匹配的本次 Run 身份");
        return;
    };
    {
        let mut state = probe.state.lock();
        if state.entered.is_some() {
            state.errors.push("本次 Run 产生多个原生进入信号".into());
            return;
        }
        state.entered = Some(captured.clone());
    }
    probe.sink.append(json!({
        "event": "native_run_entered", "run_id": probe.run_id, "phase": probe.phase,
        "session_identity_expected": identity(probe.session_identity),
        "options_identity": identity(captured.options_identity), "generation":captured.generation,
        "log_id":log_id, "location":location, "message":message,
        "thread":format!("{:?}", probe.thread_id), "tick_ns":captured.tick_ns.to_string(),
    }));
    let gate = probe.gate.lock().take();
    if let Some(gate) = gate {
        if gate.entered.send(captured.clone()).is_err() {
            probe.fail("原生进入信号无法交给取消线程");
            return;
        }
        // 本回调由原生 InferenceSession::Run 在校验输入/输出后发起；握手
        // 只用于测试消除短 Run 竞态。这里没有产品 checkpoint 或排队锁。
        match gate.accepted.recv_timeout(SIGNAL_TIMEOUT) {
            Ok(receipt)
                if receipt.accepted
                    && receipt.error.is_none()
                    && receipt.generation == Some(captured.generation)
                    && receipt.options_identity == Some(captured.options_identity) =>
            {
                probe.state.lock().cancel_tick_ns = Some(receipt.tick_ns);
            }
            Ok(_) => probe.fail("原生 Run 内取消控制未成功或身份不符"),
            Err(_) => probe.fail("原生 Run 内取消握手超时；不判为终止通过"),
        }
    }
}

struct TagScope;
impl TagScope {
    fn begin(tag: &str) -> Self {
        QA_TAG.with(|slot| assert!(slot.borrow_mut().replace(tag.to_owned()).is_none()));
        Self
    }
}
impl Drop for TagScope {
    fn drop(&mut self) {
        let _ = QA_TAG.try_with(|slot| {
            if let Ok(mut slot) = slot.try_borrow_mut() {
                *slot = None;
            }
        });
    }
}

struct ProbeScope;
impl ProbeScope {
    fn begin(probe: Arc<Probe>) -> Self {
        QA_PROBE.with(|slot| assert!(slot.borrow_mut().replace(probe).is_none()));
        Self
    }
}
impl Drop for ProbeScope {
    fn drop(&mut self) {
        let _ = QA_PROBE.try_with(|slot| {
            if let Ok(mut slot) = slot.try_borrow_mut() {
                *slot = None;
            }
        });
    }
}

struct CachedAssets {
    runtime: PathBuf,
    recognition: PathBuf,
    classes: usize,
    identities: Vec<Value>,
}

fn single_component(value: &str) -> Result<&str> {
    if value.is_empty() || value == "." || value == ".." || value.contains(['/', '\\', ':']) {
        Err("QA 固定资产名不是单一目录或文件组件".into())
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
        return Err(format!("QA 固定缓存 bytes/SHA 不符：{}", actual.display()).into());
    }
    Ok(json!({"path":actual, "bytes":bytes.len(), "sha256":sha256}))
}

fn cached_assets() -> Result<CachedAssets> {
    // 不调用 assets::ensure*，缺失/错误缓存失败，不能进入 HTTP 下载。
    let manifest: Value = serde_json::from_str(include_str!("../../dependencies/ppocr-cpu.json"))?;
    if manifest["runtime_api"] != 24 || ort::sys::ORT_API_VERSION != 24 {
        return Err("QA 仅接受固定 API24".into());
    }
    let cache = PathBuf::from(std::env::var("RUSTSHOT_OCR_NATIVE_CANCEL_QA_CACHE")?);
    if !cache.is_absolute() {
        return Err("QA 缓存路径必须为绝对路径".into());
    }
    let cache = cache.canonicalize()?;
    if cache.file_name().and_then(|name| name.to_str()) != manifest["cache_revision"].as_str() {
        return Err("QA 缓存目录与固定 cache_revision 不一致".into());
    }
    let runtime_dir = cache.join(single_component(
        manifest["runtime"]["directory"]
            .as_str()
            .ok_or("运行库目录缺失")?,
    )?);
    let model_dir = cache.join(single_component(
        manifest["models"]["directory"]
            .as_str()
            .ok_or("模型目录缺失")?,
    )?);
    let runtime_asset = manifest["runtime"]["files"]
        .as_array()
        .ok_or("运行库固定文件表缺失")?
        .iter()
        .find(|asset| asset["name"] == "onnxruntime.dll")
        .ok_or("固定运行库 DLL 缺失")?;
    let runtime = runtime_dir.join("onnxruntime.dll");
    let mut identities = vec![verify_file(&runtime, runtime_asset)?];
    for key in [
        "license",
        "detection",
        "classification",
        "recognition",
        "dictionary",
    ] {
        let asset = &manifest["models"][key];
        let name = single_component(asset["name"].as_str().ok_or("固定模型文件名缺失")?)?;
        identities.push(verify_file(&model_dir.join(name), asset)?);
    }
    let dictionary = std::fs::read_to_string(
        model_dir.join(single_component(
            manifest["models"]["dictionary"]["name"]
                .as_str()
                .ok_or("词表文件名缺失")?,
        )?),
    )?;
    let entries: Vec<_> = dictionary.trim_start_matches('\u{feff}').lines().collect();
    if entries.is_empty() || entries.len() > 200_000 || entries.iter().any(|entry| entry.is_empty())
    {
        return Err("QA 固定词表无效".into());
    }
    let classes = entries.len() + 2;
    if Some(classes as u64) != manifest["models"]["contract"]["recognition_output"][2].as_u64() {
        return Err("QA 固定词表与识别模型 CTC 类别合同不一致".into());
    }
    Ok(CachedAssets {
        runtime,
        recognition: model_dir.join(single_component(
            manifest["models"]["recognition"]["name"]
                .as_str()
                .ok_or("识别模型名缺失")?,
        )?),
        classes,
        identities,
    })
}

fn input() -> Tensor {
    let shape = vec![1, 3, 48, INPUT_WIDTH];
    let mut values = Vec::with_capacity(3 * 48 * INPUT_WIDTH);
    for channel in 0..3 {
        for y in 0..48 {
            for x in 0..INPUT_WIDTH {
                values.push(if (x / 8 + y / 6 + channel).is_multiple_of(2) {
                    -1.
                } else {
                    1.
                });
            }
        }
    }
    Tensor { shape, values }
}

fn output_contract(output: &Tensor, classes: usize) -> Result<Value> {
    match output.shape.as_slice() {
        [1, steps, actual_classes]
            if *steps > 0 && *steps <= INPUT_WIDTH && *actual_classes == classes => {}
        _ => return Err(format!("QA 识别输出形状/词表合同不符：{:?}", output.shape).into()),
    }
    if tensor_length(&output.shape)? != output.values.len()
        || output.values.iter().any(|value| !value.is_finite())
    {
        return Err("QA 识别输出长度错误或含非有限数值".into());
    }
    let mut hasher = Sha256::new();
    for value in &output.values {
        hasher.update(value.to_le_bytes());
    }
    Ok(
        json!({"shape":output.shape, "finite":true, "ctc_classes":classes, "values_sha256":format!("{:x}", hasher.finalize())}),
    )
}

fn context_and_probe(
    owner: &CancelOwner,
    session: &CpuSession,
    phase: &'static str,
    log_id: &str,
    sink: &Arc<RecordSink>,
    gate: Option<CancelGate>,
) -> Result<(TaskRunContext, Arc<Probe>)> {
    let run_id = NEXT_RUN.fetch_add(1, Ordering::Relaxed);
    let tag = format!(
        "RustShot-native-cancel-{}-{phase}-{run_id}",
        std::process::id()
    );
    let tag_scope = TagScope::begin(&tag);
    let context = TaskRunContext::new(&owner.token())?;
    drop(tag_scope);
    if context.options.tag()? != tag {
        return Err("QA cfg(test)选项钩子未注册，不能证明原生 Run 身份".into());
    }
    let probe = Arc::new(Probe {
        run_id,
        phase,
        tag,
        log_id: log_id.to_owned(),
        session_identity: session.session.ptr() as usize,
        options_identity: context.options.ptr() as usize,
        thread_id: thread::current().id(),
        state: Mutex::new(ProbeState::default()),
        gate: Mutex::new(gate),
        sink: Arc::clone(sink),
    });
    Ok((context, probe))
}

fn verify_probe(probe: &Probe, terminated: bool) {
    let state = probe.state.lock();
    assert!(state.errors.is_empty(), "{}", state.errors.join("；"));
    let entered = state
        .entered
        .as_ref()
        .expect("缺少真实原生 Run 内部进入信号");
    let returned = state
        .returned
        .as_ref()
        .expect("cfg(test)原始 status 观察钩子未注册或 Run 未返回");
    assert_eq!(returned.session_identity, probe.session_identity);
    assert_eq!(returned.options_identity, probe.options_identity);
    assert_eq!(entered.options_identity, returned.options_identity);
    assert_eq!(entered.generation, returned.generation);
    assert!(returned.generation > 0);
    assert_eq!(returned.observed_runs, 1);
    assert!(entered.tick_ns < returned.tick_ns);
    if terminated {
        let cancelled = state.cancel_tick_ns.expect("未证实在原生 Run 内接受取消");
        assert!(entered.tick_ns < cancelled && cancelled < returned.tick_ns);
        assert!(returned.status_identity.is_some_and(|value| value != 0));
        assert_eq!(
            returned.failure_options_identity,
            Some(probe.options_identity)
        );
        assert_eq!(
            returned.raw_code,
            Some(ort::sys::OrtErrorCode::ORT_FAIL as i32)
        );
        assert_eq!(returned.raw_message.as_deref(), Some(TERMINATE_MESSAGE));
    } else {
        assert!(state.cancel_tick_ns.is_none());
        assert!(returned.status_identity.is_none());
        assert!(returned.raw_code.is_none());
        assert!(returned.raw_message.is_none());
    }
}

#[test]
fn native_entry_requires_exact_tag_and_run_location() {
    let location = "inference_session.cc:2804 Run";
    assert!(native_entry_matches(
        LogLevel::Info,
        location,
        "Running with tag: current",
        "current"
    ));
    for (level, location, message) in [
        (LogLevel::Warning, location, "Running with tag: current"),
        (
            LogLevel::Info,
            "inference_session.cc:2637 PartialRun",
            "Running with tag: current",
        ),
        (
            LogLevel::Info,
            "runtime.rs:1 Run",
            "Running with tag: current",
        ),
        (LogLevel::Info, location, "Running with tag: previous"),
        (LogLevel::Info, location, "Running with tag: current extra"),
    ] {
        assert!(!native_entry_matches(level, location, message, "current"));
    }
}

#[test]
fn synthetic_rec_tensor_uses_existing_product_shape_and_budget() {
    let input = input();
    assert_eq!(input.shape, [1, 3, 48, 4096]);
    assert!(input.shape[3] <= 16384);
    assert_eq!(tensor_length(&input.shape).unwrap(), input.values.len());
    assert!(input
        .values
        .iter()
        .all(|value| value.is_finite() && (-1. ..=1.).contains(value)));
}

#[test]
#[ignore = "只由后台独占 QA 进程使用固定本机缓存执行，不下载、不访问前台"]
fn warm_native_run_termination_and_same_session_recovery() -> Result<()> {
    let sink = RecordSink::open()?;
    let assets = cached_assets()?;
    sink.append(json!({"event":"preflight", "cache":assets.identities, "classes":assets.classes,
        "runtime_api":ort::sys::ORT_API_VERSION, "runtime_version":"1.24.4", "cpu_threads":[1,1],
        "execution":"sequential", "optimization":"Level3", "input_shape":[1,3,48,INPUT_WIDTH],
        "scope":"真实 CPU Run 已进入后的终止与同一Session恢复；不资格化GUI/下载/冷加载/截图200ms/性能收益"}));
    initialize(&assets.runtime)?;
    let log_id = format!("RustShot-native-cancel-session-{}", std::process::id());
    // 与 CpuSession::new 的 CPU 配置相同；测试专属 logger/Info 不进入产品。
    let session = Session::builder()?
        .with_intra_threads(1)?
        .with_inter_threads(1)?
        .with_parallel_execution(false)?
        .with_optimization_level(GraphOptimizationLevel::Level3)?
        .with_log_id(&log_id)?
        .with_log_level(LogLevel::Info)?
        .with_logger(Arc::new(native_log))?
        .commit_from_file(&assets.recognition)?;
    let mut session = CpuSession { session };
    let session_identity = session.session.ptr() as usize;
    sink.append(json!({"event":"session_created", "session_identity":identity(session_identity)}));

    let warm_owner = CancelOwner::new();
    let (warm, warm_probe) =
        context_and_probe(&warm_owner, &session, "warm", &log_id, &sink, None)?;
    let scope = ProbeScope::begin(Arc::clone(&warm_probe));
    let warm_output = warm.runner(&mut session).run(input())?;
    drop(scope);
    let warm_output = output_contract(&warm_output, assets.classes)?;
    warm_owner.token().checkpoint()?;
    verify_probe(&warm_probe, false);
    sink.append(json!({"event":"warm_success", "run_id":warm_probe.run_id, "output":warm_output}));

    let cancel_owner = CancelOwner::new();
    let (entered_tx, entered_rx) = mpsc::channel::<EntryEvidence>();
    let (accepted_tx, accepted_rx) = mpsc::channel::<CancelReceipt>();
    let (cancel, cancel_probe) = context_and_probe(
        &cancel_owner,
        &session,
        "cancel",
        &log_id,
        &sink,
        Some(CancelGate {
            entered: entered_tx,
            accepted: accepted_rx,
        }),
    )?;
    assert_ne!(warm.options.ptr(), cancel.options.ptr());
    let scope = ProbeScope::begin(Arc::clone(&cancel_probe));
    let cancelled = thread::scope(|threads| {
        let canceller_owner = &cancel_owner;
        let canceller_sink = Arc::clone(&sink);
        let canceller_probe = Arc::clone(&cancel_probe);
        let canceller = threads.spawn(move || {
            let entry = entered_rx.recv_timeout(SIGNAL_TIMEOUT);
            let (accepted, error, generation, options_identity) = match entry {
                Ok(entry) => {
                    let accepted = canceller_owner.cancel();
                    match accepted {
                        Ok(accepted) => (
                            accepted,
                            None,
                            Some(entry.generation),
                            Some(entry.options_identity),
                        ),
                        Err(error) => (
                            false,
                            Some(error.to_string()),
                            Some(entry.generation),
                            Some(entry.options_identity),
                        ),
                    }
                }
                Err(error) => (
                    false,
                    Some(format!("真实 Run 进入信号超时：{error}")),
                    None,
                    None,
                ),
            };
            let tick_ns = canceller_sink.start.elapsed().as_nanos();
            canceller_sink.append(
                json!({"event":"cancel_control", "run_id":canceller_probe.run_id,
                "accepted":accepted, "error":error, "generation":generation,
                "options_identity":options_identity.map(identity), "tick_ns":tick_ns.to_string()}),
            );
            let receipt = CancelReceipt {
                accepted,
                error,
                generation,
                options_identity,
                tick_ns,
            };
            let _ = accepted_tx.send(receipt);
        });
        let result = cancel.runner(&mut session).run(input());
        // Run 返回之后才 join/校验；scope保证不存在失去所有者的取消线程。
        canceller.join().expect("QA 取消线程异常退出");
        result
    });
    drop(scope);
    verify_probe(&cancel_probe, true);
    let error = cancelled
        .err()
        .ok_or("已进入的原生 Run 未终止，不能资格化取消")?;
    assert!(
        is_cancelled(error.as_ref()),
        "原生终止未按产品合同分类：{error}"
    );
    assert!(cancel_owner.token().is_cancelled());
    sink.append(json!({"event":"cancelled_classified", "run_id":cancel_probe.run_id, "error":error.to_string()}));

    let recovery_owner = CancelOwner::new();
    assert!(!recovery_owner.token().is_cancelled());
    let (recovery, recovery_probe) =
        context_and_probe(&recovery_owner, &session, "recovery", &log_id, &sink, None)?;
    // warm/cancel上下文持续存活，旧RunOptions地址不可能释放后被复用。
    assert_ne!(recovery.options.ptr(), warm.options.ptr());
    assert_ne!(recovery.options.ptr(), cancel.options.ptr());
    assert_eq!(session.session.ptr() as usize, session_identity);
    let scope = ProbeScope::begin(Arc::clone(&recovery_probe));
    let recovered = recovery.runner(&mut session).run(input())?;
    drop(scope);
    let recovered = output_contract(&recovered, assets.classes)?;
    recovery_owner.token().checkpoint()?;
    verify_probe(&recovery_probe, false);
    assert_eq!(warm_output["shape"], recovered["shape"]);
    assert_eq!(warm_output["values_sha256"], recovered["values_sha256"]);
    let after = cached_assets()?;
    assert_eq!(assets.identities, after.identities);
    let record_errors = sink.errors.lock().clone();
    assert!(record_errors.is_empty(), "{}", record_errors.join("；"));
    sink.append(
        json!({"event":"recovery_success", "run_id":recovery_probe.run_id, "output":recovered,
        "same_session_identity":identity(session.session.ptr() as usize), "cache_unchanged":true}),
    );
    sink.append(json!({"event":"qualification", "ok":true, "warm_real_cpu":true,
        "native_entered_before_cancel":true, "exact_native_termination":true, "same_session_recovery":true,
        "gui":false, "network":false, "cold_loading_cancel":false, "screenshot_latency":false,
        "performance_improvement":false, "rec_only_actual_route":false}));
    let record_errors = sink.errors.lock().clone();
    assert!(record_errors.is_empty(), "{}", record_errors.join("；"));
    drop((recovery, cancel, warm));
    drop(session);
    Ok(())
}
