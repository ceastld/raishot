//! 进程内 Rust OCR：模型按需校验下载，算法不依赖 .NET 或外部进程。
pub(crate) mod assets;
#[cfg(test)]
mod bench_tests;
mod discovery;
pub(crate) mod download_progress;
mod download_source;
mod engine_cache;
#[cfg(test)]
mod kernel_probe_tests;
#[cfg(test)]
mod kernels;
mod pipeline;
pub(crate) mod profile;
mod runtime;
pub(crate) mod table;
mod table_structure;
mod threads;
mod warmup;
pub(crate) use profile::OcrProfile;
pub(crate) use warmup::WarmupMode;

use crate::{
    capture::Image,
    geometry::Rect,
    recognition::{
        task::{wait_for_lock, CancelOwner, TaskToken},
        RecognizedText,
    },
    Result,
};
use parking_lot::Mutex;
use std::{sync::OnceLock, time::Instant};
use warmup::{elapsed_ms, Timing};

pub(crate) struct Tensor {
    pub shape: Vec<usize>,
    pub values: Vec<f32>,
}

pub(crate) trait ModelRunner {
    fn run(&mut self, input: Tensor) -> Result<Tensor>;
}

struct Engines {
    profile: OcrProfile,
    intra_threads: usize,
    det: runtime::CpuSession,
    recognition: Option<RecognitionEngine>,
}

struct RecognitionEngine {
    cls: runtime::CpuSession,
    rec: runtime::CpuSession,
    dictionary: Vec<String>,
}

// 一次性截图工作进程可在 WarmReady 后只读缓存预热；托盘不加载模型。
// 活动请求与预热共用进程内互斥引擎，不保留图像或创建 GPUI / 应用 GPU 设备。
static ENGINES: OnceLock<Mutex<engine_cache::EngineCache<Engines>>> = OnceLock::new();

fn validate_image(image: &Image) -> Result<()> {
    let pixels = (image.width as usize).checked_mul(image.height as usize);
    let expected = pixels.and_then(|pixels| pixels.checked_mul(4));
    if image.width == 0 || image.height == 0 || expected != Some(image.bgra.len()) {
        return Err("OCR 图像尺寸与 BGRA 数据不一致".into());
    }
    if pixels.is_none_or(|pixels| pixels > 32_000_000) {
        return Err(tr!(
            "OCR 图像超过工作区预算，请缩小识别范围",
            "The image is too large for OCR; select a smaller area"
        )
        .into());
    }
    Ok(())
}

fn engine<'a>(
    state: &'a mut engine_cache::EngineCache<Engines>,
    profile: OcrProfile,
    task: &TaskToken,
    timing: &mut Timing,
) -> Result<&'a mut Engines> {
    task.checkpoint()?;
    let intra_threads = threads::current();
    if state
        .entry(profile)
        .as_ref()
        .is_none_or(|engine| engine.intra_threads != intra_threads)
    {
        let started = Instant::now();
        let assets = match profile {
            OcrProfile::Tiny => assets::ensure_det_cancellable(task)?,
            OcrProfile::Small => assets::ensure_det_profile_cancellable(profile, task)?,
        };
        timing.add_assets(started);
        return engine_from_assets(state, profile, &assets, intra_threads, task, timing);
    }
    state
        .entry_mut(profile)
        .as_mut()
        .ok_or_else(|| "OCR 引擎未初始化".into())
}

fn engine_from_assets<'a>(
    state: &'a mut engine_cache::EngineCache<Engines>,
    profile: OcrProfile,
    assets: &assets::OcrDetectionAssets,
    intra_threads: usize,
    task: &TaskToken,
    timing: &mut Timing,
) -> Result<&'a mut Engines> {
    task.checkpoint()?;
    if assets.profile != profile {
        return Err("OCR 检测资产与请求 profile 不匹配".into());
    }
    if state
        .entry(profile)
        .as_ref()
        .is_none_or(|engine| engine.intra_threads != intra_threads)
    {
        // 原生初始化不能中途打断；返回后先核取消，再传播初始化结果。
        let started = Instant::now();
        let initialized = runtime::initialize(&assets.runtime);
        timing.runtime_ms = Some(elapsed_ms(started));
        task.checkpoint()?;
        initialized?;
        let started = Instant::now();
        let cpu = warmup::thread_cpu_ms();
        let environment = runtime::environment();
        timing.environment_ms = Some(elapsed_ms(started));
        timing.environment_cpu_ms = cpu.zip(warmup::thread_cpu_ms()).map(|(a, b)| b - a);
        task.checkpoint()?;
        environment?;
        // 候选 session 构造成功才发布；失败由 RAII 释放，可再次重试。
        let started = Instant::now();
        let det = runtime::CpuSession::new(&assets.detection, intra_threads);
        timing.detection_ms = Some(elapsed_ms(started));
        task.checkpoint()?;
        let det = det?;
        state.publish_cancellable(
            profile,
            Engines {
                profile,
                intra_threads,
                det,
                recognition: None,
            },
            task,
        )?;
    }
    state
        .entry_mut(profile)
        .as_mut()
        .ok_or_else(|| "OCR 引擎未初始化".into())
}

/// 仅在固定缓存全命中时准备 CPU 引擎，无图像、无下载；任务失败仍保留按需入口。
pub(crate) fn prepare_cached_cancellable(task: &TaskToken) -> Result<bool> {
    prepare_cached_profile_cancellable(OcrProfile::Tiny, task)
}

/// 仅准备调用者明确选择的 profile；不遍历其它档位，不触发缺失文件下载。
pub(crate) fn prepare_cached_profile_cancellable(
    profile: OcrProfile,
    task: &TaskToken,
) -> Result<bool> {
    let started = Instant::now();
    let mut timing = Timing::default();
    let cached = match profile {
        OcrProfile::Tiny => assets::cached_cancellable(task)?,
        OcrProfile::Small => assets::cached_profile_cancellable(profile, task)?,
    };
    timing.add_assets(started);
    let Some(assets) = cached else {
        return Ok(false);
    };
    task.checkpoint()?;
    let locked = Instant::now();
    let mut state = wait_for_lock(
        ENGINES.get_or_init(|| Mutex::new(engine_cache::EngineCache::default())),
        task,
    )?;
    timing.lock_wait_ms = elapsed_ms(locked);
    let detection_assets = assets::OcrDetectionAssets {
        profile: assets.profile,
        runtime: assets.runtime.clone(),
        detection: assets.detection.clone(),
    };
    let engine = engine_from_assets(
        &mut state,
        profile,
        &detection_assets,
        threads::current(),
        task,
        &mut timing,
    )?;
    prepare_recognition(engine, &assets, task, &mut timing)?;
    task.checkpoint()?;
    drop(state);
    timing.record("warmup", profile, started);
    Ok(true)
}

/// Owner 由宿主持有至截图结束或进程退出；只将令牌交给后台，不在请求链 join。
/// `delay` 让贴图等宿主先完成首显；期间到来的识别请求自行初始化，预热随后直接复用。
pub(crate) fn start_cached_profile_warmup(
    profile: OcrProfile,
    delay: std::time::Duration,
) -> Option<CancelOwner> {
    let owner = CancelOwner::new();
    let task = owner.token();
    let spawned = std::thread::Builder::new()
        .name("rustshot-ocr-warmup".into())
        .spawn(move || {
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
            let prepared = match profile {
                OcrProfile::Tiny => prepare_cached_cancellable(&task),
                OcrProfile::Small => prepare_cached_profile_cancellable(profile, &task),
            };
            if let Err(error) = prepared {
                if !crate::recognition::task::is_cancelled(error.as_ref()) {
                    eprintln!("OCR CPU 缓存预热未完成，保留按需识别：{error}");
                }
            }
        });
    match spawned {
        Ok(_) => Some(owner),
        Err(error) => {
            eprintln!("OCR CPU 预热线程未启动，保留按需识别：{error}");
            None
        }
    }
}

fn prepare_recognition(
    engine: &mut Engines,
    assets: &assets::OcrAssets,
    task: &TaskToken,
    timing: &mut Timing,
) -> Result<()> {
    task.checkpoint()?;
    if engine.profile != assets.profile {
        return Err("OCR 识别资产与检测引擎 profile 不匹配".into());
    }
    if engine.recognition.is_some() {
        return Ok(());
    }
    let started = Instant::now();
    let initialized = runtime::initialize(&assets.runtime);
    task.checkpoint()?;
    initialized?;
    // OS 文件读取同样是同步阶段；完成后取消优先于读失败与后续解析。
    let dictionary = std::fs::read_to_string(&assets.dictionary);
    task.checkpoint()?;
    let dictionary = dictionary?;
    let dictionary: Vec<String> = dictionary
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::to_owned)
        .collect();
    task.checkpoint()?;
    let validated = engine.profile.validate_dictionary(&dictionary);
    task.checkpoint()?;
    validated?;
    task.checkpoint()?;
    let cls = runtime::CpuSession::new(&assets.classification, engine.intra_threads);
    task.checkpoint()?;
    let cls = cls?;
    let rec = runtime::CpuSession::new(&assets.recognition, engine.intra_threads);
    task.checkpoint()?;
    let rec = rec?;
    timing.recognition_ms = Some(elapsed_ms(started));
    // 两个候选 session 全部成功才发布；中途失败不会遗留半套识别引擎。
    engine.recognition = Some(RecognitionEngine {
        cls,
        rec,
        dictionary,
    });
    Ok(())
}
#[cfg(test)]
pub(crate) fn detect(image: &Image) -> Result<Vec<Rect>> {
    let owner = CancelOwner::new();
    detect_cancellable(image, &owner.token())
}

#[cfg(test)]
pub(crate) fn detect_cancellable(image: &Image, task: &TaskToken) -> Result<Vec<Rect>> {
    detect_profile_cancellable(image, OcrProfile::Tiny, task)
}

pub(crate) fn detect_profile_cancellable(
    image: &Image,
    profile: OcrProfile,
    task: &TaskToken,
) -> Result<Vec<Rect>> {
    validate_image(image)?;
    let started = Instant::now();
    let mut timing = Timing::default();
    let mut state = wait_for_lock(
        ENGINES.get_or_init(|| Mutex::new(engine_cache::EngineCache::default())),
        task,
    )?;
    timing.lock_wait_ms = elapsed_ms(started);
    let engine = engine(&mut state, profile, task, &mut timing)?;
    let run = runtime::TaskRunContext::new(task)?;
    let mut det = run.runner(&mut engine.det);
    let run_started = Instant::now();
    let result = pipeline::detect_cancellable(image, &mut det, task)?
        .into_iter()
        .filter(|line| line.confidence >= 0.5)
        .map(|line| line.rect)
        .collect();
    timing.run_ms = Some(elapsed_ms(run_started));
    task.checkpoint()?;
    drop(det);
    drop(run);
    drop(state);
    timing.record("detect", profile, started);
    Ok(result)
}

pub(crate) fn recognize(image: &Image) -> Result<RecognizedText> {
    let owner = CancelOwner::new();
    recognize_cancellable(image, &owner.token())
}

pub(crate) fn recognize_cancellable(image: &Image, task: &TaskToken) -> Result<RecognizedText> {
    recognize_profile_cancellable(image, OcrProfile::Tiny, task)
}

pub(crate) fn recognize_profile_cancellable(
    image: &Image,
    profile: OcrProfile,
    task: &TaskToken,
) -> Result<RecognizedText> {
    validate_image(image)?;
    let started = Instant::now();
    let mut timing = Timing::default();
    let mut state = wait_for_lock(
        ENGINES.get_or_init(|| Mutex::new(engine_cache::EngineCache::default())),
        task,
    )?;
    timing.lock_wait_ms = elapsed_ms(started);
    let engine = engine(&mut state, profile, task, &mut timing)?;
    task.checkpoint()?;
    if engine.recognition.is_none() {
        let assets_started = Instant::now();
        let assets = match profile {
            OcrProfile::Tiny => assets::ensure_cancellable(task)?,
            OcrProfile::Small => assets::ensure_profile_cancellable(profile, task)?,
        };
        timing.add_assets(assets_started);
        prepare_recognition(engine, &assets, task, &mut timing)?;
    }
    let run = runtime::TaskRunContext::new(task)?;
    let recognition = engine.recognition.as_mut().ok_or("OCR 识别模型未初始化")?;
    let mut det = run.runner(&mut engine.det);
    let mut cls = run.runner(&mut recognition.cls);
    let mut rec = run.runner(&mut recognition.rec);
    let run_started = Instant::now();
    let result = pipeline::recognize_profile_cancellable(
        image,
        &mut det,
        &mut cls,
        &mut rec,
        &recognition.dictionary,
        profile,
        task,
    )?;
    timing.run_ms = Some(elapsed_ms(run_started));
    task.checkpoint()?;
    drop((det, cls, rec));
    drop(run);
    drop(state);
    timing.record("recognize", profile, started);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_image_is_rejected_before_loading_or_downloading_dependencies() {
        let bad = Image {
            width: u32::MAX,
            height: u32::MAX,
            bgra: Vec::new(),
            readback_ms: 0.,
        };
        assert!(validate_image(&bad).is_err());
        assert!(detect(&bad).is_err());
        assert!(recognize(&bad).is_err());
        let empty = Image {
            width: 0,
            height: 1,
            bgra: Vec::new(),
            readback_ms: 0.,
        };
        assert!(validate_image(&empty).is_err());
    }
}
