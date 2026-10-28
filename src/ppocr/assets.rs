//! 按需准备独立 CPU OCR 资产；仅由 OCR 后台任务调用，不能在托盘或冻结阶段调用。
//! 取消为分块合作式：HTTP connect／headers、单次 Read／Write、文件同步／移动及
//! 原生 DLL 加载在返回之前不能被令牌即时打断。返回时本调用拥有的流已关闭；没有后台下载残留。
use super::{
    download_progress,
    download_source::{self, Source},
    profile::OcrProfile,
};
use crate::{recognition::task::TaskToken, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, MutexGuard, TryLockError,
    },
    time::{Duration, Instant},
};
use windows::{
    core::{HSTRING, PCWSTR},
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PATH_NOT_FOUND},
        Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH},
        System::{
            Registry::{
                RegGetValueW, HKEY_CURRENT_USER, REG_ROUTINE_FLAGS, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
            },
            SystemInformation::GetSystemDirectoryW,
        },
    },
};

static PREPARATION: Mutex<()> = Mutex::new(());
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct OcrAssets {
    pub profile: OcrProfile,
    pub runtime: PathBuf,
    pub detection: PathBuf,
    pub classification: PathBuf,
    pub recognition: PathBuf,
    pub dictionary: PathBuf,
}

#[derive(Clone, Debug)]
pub struct OcrDetectionAssets {
    pub profile: OcrProfile,
    pub runtime: PathBuf,
    pub detection: PathBuf,
}

#[derive(Clone, Debug)]
pub struct TableAssets {
    pub runtime: PathBuf,
    pub model: PathBuf,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    url: String,
    bytes: u64,
    sha256: String,
    /// 官方备用来源；未给 `upstream_bytes`／`upstream_sha256` 时与镜像字节相同。
    #[serde(default)]
    upstream_url: Option<String>,
    #[serde(default)]
    upstream_bytes: Option<u64>,
    #[serde(default)]
    upstream_sha256: Option<String>,
}

impl Asset {
    /// 清单顺序：主源在前，备用源在后；相同地址与文件身份只尝试一次。
    fn sources(&self) -> Vec<Source<'_>> {
        let mut sources = vec![Source {
            url: &self.url,
            name: &self.name,
            bytes: self.bytes,
            sha256: &self.sha256,
        }];
        if let Some(url) = self.upstream_url.as_deref() {
            match (self.upstream_bytes, self.upstream_sha256.as_deref()) {
                (Some(bytes), Some(sha256)) => {
                    // 身份不同的官方包按 URL 末段命名落盘，不覆盖镜像文件名。
                    if let Some(name) = upstream_name(url) {
                        sources.push(Source {
                            url,
                            name,
                            bytes,
                            sha256,
                        });
                    }
                }
                _ => sources.push(Source { url, ..sources[0] }),
            }
        }
        sources.dedup_by(|left, right| {
            left.url == right.url
                && left.name == right.name
                && left.bytes == right.bytes
                && left.sha256 == right.sha256
        });
        sources
    }

    fn validate_sources(&self) -> Result<()> {
        for source in self.sources() {
            valid_name(source.name)?;
            valid_identity(source.bytes, source.sha256)?;
            if !source.url.starts_with("https://") {
                return Err("OCR 固定来源必须使用 HTTPS".into());
            }
        }
        if self.upstream_url.as_deref().is_some_and(|url| {
            self.upstream_bytes.is_some()
                && self.upstream_sha256.is_some()
                && upstream_name(url).is_none()
        }) {
            return Err("OCR 官方来源文件名无效".into());
        }
        Ok(())
    }
}

fn upstream_name(url: &str) -> Option<&str> {
    let path = url.split(['?', '#']).next()?;
    path.rsplit('/')
        .next()
        .filter(|name| valid_name(name).is_ok())
}
#[derive(Deserialize)]
struct ArchiveFile {
    name: String,
    member: String,
    bytes: u64,
    sha256: String,
}
#[derive(Deserialize)]
struct Runtime {
    directory: String,
    archive: Asset,
    files: Vec<ArchiveFile>,
}
#[derive(Deserialize)]
struct Models {
    profile: String,
    directory: String,
    detection: Asset,
    classification: Asset,
    recognition: Asset,
    dictionary: Asset,
    license: Asset,
}
impl Models {
    fn detection_assets(&self) -> [&Asset; 2] {
        [&self.license, &self.detection]
    }

    fn recognition_assets(&self) -> [&Asset; 5] {
        [
            &self.license,
            &self.detection,
            &self.classification,
            &self.recognition,
            &self.dictionary,
        ]
    }
}
#[derive(Deserialize)]
struct Manifest {
    cache_revision: String,
    runtime_api: u32,
    runtime: Runtime,
    models: Models,
}

#[derive(Deserialize)]
struct TableManifest {
    cache_revision: String,
    runtime_api: u32,
    runtime_manifest: String,
    models: TableModels,
}

#[derive(Deserialize)]
struct TableModels {
    profile: String,
    directory: String,
    structure: Asset,
    license: Asset,
}

/// 表格结构模型单独按需下载；只共享 ORT，不准备文字 det／cls／rec。
pub fn ensure_table_cancellable(task: &TaskToken) -> Result<TableAssets> {
    let task = Some(task);
    let _guard = wait_for_preparation(&PREPARATION, task)?;
    let table = checked(task, table_manifest)?;
    let (manifest, locations) = profile_preparation_cancellable(OcrProfile::Tiny, task)?;
    let root = checked(task, || {
        Ok(std::path::absolute(crate::preferences::data_dir())?)
    })?
    .join("ocr")
    .join(&table.cache_revision)
    .join(&table.models.directory);
    let _progress = download_progress::plan(
        runtime_missing_bytes(&manifest, &locations)
            + missing_bytes([
                (
                    root.join(&table.models.license.name).as_path(),
                    &table.models.license,
                ),
                (
                    root.join(&table.models.structure.name).as_path(),
                    &table.models.structure,
                ),
            ]),
    );
    let runtime = prepare_locations_runtime_cancellable(&manifest, &locations, task)?;
    download_at_cancellable(
        &root.join(&table.models.license.name),
        &table.models.license,
        task,
    )?;
    let model = download_at_cancellable(
        &root.join(&table.models.structure.name),
        &table.models.structure,
        task,
    )?;
    checkpoint(task)?;
    Ok(TableAssets { runtime, model })
}

/// 与每次 OCR 请求共用取消令牌；不为下载另建工作线程。
#[cfg(test)]
pub fn ensure_runtime_cancellable(task: &TaskToken) -> Result<PathBuf> {
    ensure_runtime_with_task(Some(task))
}

#[cfg(test)]
fn ensure_runtime_with_task(task: Option<&TaskToken>) -> Result<PathBuf> {
    let _guard = wait_for_preparation(&PREPARATION, task)?;
    let (manifest, root) = preparation_cancellable(task)?;
    let _progress = download_progress::plan(0);
    let runtime = prepare_runtime_cancellable(&manifest, &root, task)?;
    checkpoint(task)?;
    Ok(runtime)
}

/// 查找文字区域只准备检测器；不下载 cls、rec 或字典。
#[allow(dead_code)] // 保留无令牌兼容入口，常规请求现由 cancellable 入口消费。
pub fn ensure_det() -> Result<OcrDetectionAssets> {
    ensure_det_profile_with_task(OcrProfile::Tiny, None)
}

pub fn ensure_det_cancellable(task: &TaskToken) -> Result<OcrDetectionAssets> {
    ensure_det_profile_cancellable(OcrProfile::Tiny, task)
}

pub fn ensure_det_profile_cancellable(
    profile: OcrProfile,
    task: &TaskToken,
) -> Result<OcrDetectionAssets> {
    ensure_det_profile_with_task(profile, Some(task))
}

fn ensure_det_profile_with_task(
    profile: OcrProfile,
    task: Option<&TaskToken>,
) -> Result<OcrDetectionAssets> {
    let _guard = wait_for_preparation(&PREPARATION, task)?;
    let (manifest, locations) = profile_preparation_cancellable(profile, task)?;
    let [license, detection] = manifest.models.detection_assets();
    let _progress = download_progress::plan(
        runtime_missing_bytes(&manifest, &locations)
            + missing_bytes([
                (locations.license.as_path(), license),
                (locations.detection.as_path(), detection),
            ]),
    );
    let runtime = prepare_locations_runtime_cancellable(&manifest, &locations, task)?;
    download_at_cancellable(&locations.license, license, task)?;
    let detection = download_at_cancellable(&locations.detection, detection, task)?;
    checkpoint(task)?;
    Ok(OcrDetectionAssets {
        profile,
        runtime,
        detection,
    })
}

/// 完整中英文字识别按需准备 det、cls、rec 和固定字典。
#[allow(dead_code)] // 保留无令牌兼容入口，常规请求现由 cancellable 入口消费。
pub fn ensure() -> Result<OcrAssets> {
    ensure_profile_with_task(OcrProfile::Tiny, None)
}

pub fn ensure_cancellable(task: &TaskToken) -> Result<OcrAssets> {
    ensure_profile_cancellable(OcrProfile::Tiny, task)
}

pub fn ensure_profile_cancellable(profile: OcrProfile, task: &TaskToken) -> Result<OcrAssets> {
    ensure_profile_with_task(profile, Some(task))
}

fn ensure_profile_with_task(profile: OcrProfile, task: Option<&TaskToken>) -> Result<OcrAssets> {
    let _guard = wait_for_preparation(&PREPARATION, task)?;
    let (manifest, locations) = profile_preparation_cancellable(profile, task)?;
    let _progress = download_progress::plan(
        runtime_missing_bytes(&manifest, &locations)
            + missing_bytes(locations.model_files(&manifest.models)),
    );
    let runtime = prepare_locations_runtime_cancellable(&manifest, &locations, task)?;
    let [license, detection, classification, recognition, dictionary] =
        manifest.models.recognition_assets();
    download_at_cancellable(&locations.license, license, task)?;
    let detection = download_at_cancellable(&locations.detection, detection, task)?;
    let classification = download_at_cancellable(&locations.classification, classification, task)?;
    let recognition = download_at_cancellable(&locations.recognition, recognition, task)?;
    let dictionary = download_at_cancellable(&locations.dictionary, dictionary, task)?;
    let assets = OcrAssets {
        profile,
        runtime,
        detection,
        classification,
        recognition,
        dictionary,
    };
    if [
        &assets.runtime,
        &assets.detection,
        &assets.classification,
        &assets.recognition,
        &assets.dictionary,
    ]
    .iter()
    .any(|path| !path.is_absolute())
    {
        return Err("OCR 依赖必须发布绝对路径".into());
    }
    checkpoint(task)?;
    Ok(assets)
}

/// 待命只读路径：全部固定文件已经缓存时才预热；不创建目录、不下载、不解压。
/// 缺文件返回 None；已有损坏文件保留正常错误，后台调用者不会覆盖缓存。
pub(crate) fn cached_cancellable(task: &TaskToken) -> Result<Option<OcrAssets>> {
    cached_profile_cancellable(OcrProfile::Tiny, task)
}

/// Small 只有显式选中才探测，缺文件不借用 Tiny det/rec/dict，也不补下载。
pub(crate) fn cached_profile_cancellable(
    profile: OcrProfile,
    task: &TaskToken,
) -> Result<Option<OcrAssets>> {
    task.checkpoint()?;
    if !cfg!(target_arch = "x86_64") {
        return Err("本 OCR CPU 依赖目前仅支持 Windows x64".into());
    }
    let _guard = wait_for_preparation(&PREPARATION, Some(task))?;
    let manifest = checked(Some(task), || manifest_for(profile))?;
    let tiny = match profile {
        OcrProfile::Tiny => None,
        OcrProfile::Small => Some(checked(Some(task), || manifest_for(OcrProfile::Tiny))?),
    };
    let ocr_root = checked(Some(task), || {
        Ok(std::path::absolute(crate::preferences::data_dir())?)
    })?
    .join("ocr");
    let locations = cache_locations(profile, &manifest, tiny.as_ref(), &ocr_root)?;
    let locations = with_bundled_runtime(&manifest, locations, Some(task))?;
    let cached = cached_locations_cancellable(profile, &manifest, &locations, task)?;
    if cached.is_some() {
        check_visual_cpp_cancellable(Some(task))?;
    }
    task.checkpoint()?;
    Ok(cached)
}

#[cfg(test)]
fn cached_at_cancellable(
    manifest: &Manifest,
    root: &Path,
    task: &TaskToken,
) -> Result<Option<OcrAssets>> {
    task.checkpoint()?;
    if !root.is_absolute() {
        return Err("OCR 缓存探测必须使用绝对路径".into());
    }
    let runtime = root.join(&manifest.runtime.directory);
    for file in &manifest.runtime.files {
        if !verified_cancellable(
            &runtime.join(&file.name),
            file.bytes,
            &file.sha256,
            Some(task),
        )? {
            return Ok(None);
        }
    }
    let models = root.join(&manifest.models.directory);
    for asset in manifest.models.recognition_assets() {
        if !verified_cancellable(
            &models.join(&asset.name),
            asset.bytes,
            &asset.sha256,
            Some(task),
        )? {
            return Ok(None);
        }
    }
    task.checkpoint()?;
    Ok(Some(OcrAssets {
        profile: OcrProfile::Tiny,
        runtime: runtime.join("onnxruntime.dll"),
        detection: models.join(&manifest.models.detection.name),
        classification: models.join(&manifest.models.classification.name),
        recognition: models.join(&manifest.models.recognition.name),
        dictionary: models.join(&manifest.models.dictionary.name),
    }))
}

#[derive(Debug)]
struct CacheLocations {
    runtime_root: PathBuf,
    runtime: PathBuf,
    /// 运行库来自发布包（EXE 同目录）且已按固定清单核对，不再从缓存下载。
    bundled_runtime: bool,
    detection: PathBuf,
    classification: PathBuf,
    recognition: PathBuf,
    dictionary: PathBuf,
    license: PathBuf,
}

impl CacheLocations {
    fn model_files<'a>(&'a self, models: &'a Models) -> [(&'a Path, &'a Asset); 5] {
        [
            (&self.license, &models.license),
            (&self.detection, &models.detection),
            (&self.classification, &models.classification),
            (&self.recognition, &models.recognition),
            (&self.dictionary, &models.dictionary),
        ]
    }
}

/// 路径是固定清单身份的结果；不从目录名推断模型或用 Tiny 代替 Small。
fn cache_locations(
    profile: OcrProfile,
    manifest: &Manifest,
    tiny: Option<&Manifest>,
    ocr_root: &Path,
) -> Result<CacheLocations> {
    if !ocr_root.is_absolute() {
        return Err("OCR 缓存探测必须使用绝对路径".into());
    }
    validate_profile_label(profile, manifest)?;
    let own_root = ocr_root.join(&manifest.cache_revision);
    let own_models = own_root.join(&manifest.models.directory);
    let (runtime_root, classification, license) = match profile {
        OcrProfile::Tiny => (
            own_root.clone(),
            own_models.join(&manifest.models.classification.name),
            own_models.join(&manifest.models.license.name),
        ),
        OcrProfile::Small => {
            let tiny = tiny.ok_or("Small 缺少共享运行库身份基准")?;
            validate_profile_label(OcrProfile::Tiny, tiny)?;
            if manifest.cache_revision == tiny.cache_revision
                || manifest.models.directory == tiny.models.directory
            {
                return Err("Small 模型必须使用独立缓存身份".into());
            }
            if !same_runtime(&manifest.runtime, &tiny.runtime) {
                return Err("Small 与 Tiny 的共享运行库固定身份不一致".into());
            }
            if [
                (&manifest.models.detection, &tiny.models.detection),
                (&manifest.models.recognition, &tiny.models.recognition),
                (&manifest.models.dictionary, &tiny.models.dictionary),
            ]
            .iter()
            .any(|(small, tiny)| same_content(small, tiny))
            {
                return Err("Small det/rec/字典不能静默复用 Tiny 资产".into());
            }
            let tiny_root = ocr_root.join(&tiny.cache_revision);
            let tiny_models = tiny_root.join(&tiny.models.directory);
            let classification =
                if same_asset(&manifest.models.classification, &tiny.models.classification) {
                    tiny_models.join(&tiny.models.classification.name)
                } else {
                    own_models.join(&manifest.models.classification.name)
                };
            let license = if same_asset(&manifest.models.license, &tiny.models.license) {
                tiny_models.join(&tiny.models.license.name)
            } else {
                own_models.join(&manifest.models.license.name)
            };
            (tiny_root, classification, license)
        }
    };
    Ok(CacheLocations {
        runtime: runtime_root
            .join(&manifest.runtime.directory)
            .join("onnxruntime.dll"),
        bundled_runtime: false,
        runtime_root,
        detection: own_models.join(&manifest.models.detection.name),
        classification,
        recognition: own_models.join(&manifest.models.recognition.name),
        dictionary: own_models.join(&manifest.models.dictionary.name),
        license,
    })
}

fn same_asset(left: &Asset, right: &Asset) -> bool {
    left.name == right.name && same_content(left, right)
}

fn same_content(left: &Asset, right: &Asset) -> bool {
    left.bytes == right.bytes && left.sha256.eq_ignore_ascii_case(&right.sha256)
}

fn same_runtime(left: &Runtime, right: &Runtime) -> bool {
    left.directory == right.directory
        && same_asset(&left.archive, &right.archive)
        && left.files.len() == right.files.len()
        && left.files.iter().zip(&right.files).all(|(left, right)| {
            left.name == right.name
                && left.member == right.member
                && left.bytes == right.bytes
                && left.sha256 == right.sha256
        })
}

fn cached_locations_cancellable(
    profile: OcrProfile,
    manifest: &Manifest,
    locations: &CacheLocations,
    task: &TaskToken,
) -> Result<Option<OcrAssets>> {
    task.checkpoint()?;
    let runtime_directory = locations.runtime.parent().ok_or("OCR 运行库路径缺少目录")?;
    let runtime_files = if locations.bundled_runtime {
        &[][..]
    } else {
        &manifest.runtime.files[..]
    };
    for file in runtime_files {
        if !verified_cancellable(
            &runtime_directory.join(&file.name),
            file.bytes,
            &file.sha256,
            Some(task),
        )? {
            return Ok(None);
        }
    }
    for (path, asset) in locations.model_files(&manifest.models) {
        if !verified_cancellable(path, asset.bytes, &asset.sha256, Some(task))? {
            return Ok(None);
        }
    }
    task.checkpoint()?;
    Ok(Some(OcrAssets {
        profile,
        runtime: locations.runtime.clone(),
        detection: locations.detection.clone(),
        classification: locations.classification.clone(),
        recognition: locations.recognition.clone(),
        dictionary: locations.dictionary.clone(),
    }))
}

fn profile_preparation_cancellable(
    profile: OcrProfile,
    task: Option<&TaskToken>,
) -> Result<(Manifest, CacheLocations)> {
    checkpoint(task)?;
    if !cfg!(target_arch = "x86_64") {
        return Err("本 OCR CPU 依赖目前仅支持 Windows x64".into());
    }
    let manifest = checked(task, || manifest_for(profile))?;
    let tiny = match profile {
        OcrProfile::Tiny => None,
        OcrProfile::Small => Some(checked(task, || manifest_for(OcrProfile::Tiny))?),
    };
    let ocr_root = checked(task, || {
        Ok(std::path::absolute(crate::preferences::data_dir())?)
    })?
    .join("ocr");
    let locations = cache_locations(profile, &manifest, tiny.as_ref(), &ocr_root)?;
    let locations = with_bundled_runtime(&manifest, locations, task)?;
    check_visual_cpp_cancellable(task)?;
    checkpoint(task)?;
    Ok((manifest, locations))
}

/// 只看文件是否存在来估算下载总量，不哈希；已有文件的校验仍在下载入口进行。
fn missing_bytes<'a>(files: impl IntoIterator<Item = (&'a Path, &'a Asset)>) -> u64 {
    files
        .into_iter()
        .filter(|(path, _)| !path.try_exists().unwrap_or(false))
        .map(|(_, asset)| asset.bytes)
        .sum()
}

fn runtime_missing_bytes(manifest: &Manifest, locations: &CacheLocations) -> u64 {
    let directory = locations.runtime_root.join(&manifest.runtime.directory);
    let archive = &manifest.runtime.archive;
    if locations.bundled_runtime
        || manifest
            .runtime
            .files
            .iter()
            .all(|file| directory.join(&file.name).try_exists().unwrap_or(false))
        || locations
            .runtime_root
            .join(&archive.name)
            .try_exists()
            .unwrap_or(false)
    {
        return 0;
    }
    archive.bytes
}

fn download_at_cancellable(
    destination: &Path,
    asset: &Asset,
    task: Option<&TaskToken>,
) -> Result<PathBuf> {
    checkpoint(task)?;
    if destination.file_name() != Some(std::ffi::OsStr::new(&asset.name)) {
        return Err("OCR 资产发布路径与清单不匹配".into());
    }
    let directory = destination.parent().ok_or("OCR 资产路径缺少目录")?;
    checked(task, || Ok(fs::create_dir_all(directory)?))?;
    download_cancellable(directory, asset, task)
}

fn checkpoint(task: Option<&TaskToken>) -> Result<()> {
    task.map_or(Ok(()), TaskToken::checkpoint)
}

/// 同步调用返回后先保留取消类型，再处理其普通错误；该调用本身仍可能阻塞。
fn checked<T>(task: Option<&TaskToken>, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    checkpoint(task)?;
    let result = operation();
    checkpoint(task)?;
    result
}

/// std::Mutex 的 poison 仍是错误；排队者每 10ms 检查本请求，不影响持锁任务。
fn wait_for_preparation<'a, T>(
    mutex: &'a Mutex<T>,
    task: Option<&TaskToken>,
) -> Result<MutexGuard<'a, T>> {
    if task.is_none() {
        return mutex.lock().map_err(|_| "OCR 依赖准备锁失效".into());
    }
    loop {
        checkpoint(task)?;
        match mutex.try_lock() {
            Ok(guard) => {
                checkpoint(task)?;
                return Ok(guard);
            }
            Err(TryLockError::Poisoned(_)) => {
                checkpoint(task)?;
                return Err("OCR 依赖准备锁失效".into());
            }
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[cfg(test)]
fn manifest() -> Result<Manifest> {
    manifest_for(OcrProfile::Tiny)
}

fn manifest_for(profile: OcrProfile) -> Result<Manifest> {
    let source = match profile {
        OcrProfile::Tiny => include_str!("../../dependencies/ppocr-cpu.json"),
        OcrProfile::Small => include_str!("../../dependencies/ppocr-small-cpu.json"),
    };
    let manifest: Manifest = serde_json::from_str(source)?;
    if manifest.runtime_api != 24 {
        return Err("OCR 清单的原生 API 版本不匹配".into());
    }
    valid_name(&manifest.cache_revision)?;
    valid_name(&manifest.runtime.directory)?;
    valid_name(&manifest.models.directory)?;
    for asset in [
        &manifest.runtime.archive,
        &manifest.models.detection,
        &manifest.models.classification,
        &manifest.models.recognition,
        &manifest.models.dictionary,
        &manifest.models.license,
    ] {
        asset.validate_sources()?;
    }
    for file in &manifest.runtime.files {
        valid_name(&file.name)?;
        valid_identity(file.bytes, &file.sha256)?;
    }
    validate_profile_label(profile, &manifest)?;
    Ok(manifest)
}

fn table_manifest() -> Result<TableManifest> {
    let manifest: TableManifest =
        serde_json::from_str(include_str!("../../dependencies/ppocr-table-cpu.json"))?;
    if manifest.runtime_api != 24 || manifest.runtime_manifest != "ppocr-cpu.json" {
        return Err("表格清单的共享原生运行库身份不匹配".into());
    }
    if manifest.models.profile != "SLANet_plus" {
        return Err("表格清单的结构模型身份不匹配".into());
    }
    valid_name(&manifest.cache_revision)?;
    valid_name(&manifest.models.directory)?;
    for asset in [&manifest.models.structure, &manifest.models.license] {
        asset.validate_sources()?;
    }
    Ok(manifest)
}

fn validate_profile_label(profile: OcrProfile, manifest: &Manifest) -> Result<()> {
    let label = match profile {
        OcrProfile::Tiny => "PP-OCRv6-tiny",
        OcrProfile::Small => "PP-OCRv6-small",
    };
    if manifest.models.profile != label {
        return Err("OCR 清单与显式 profile 不匹配".into());
    }
    Ok(())
}

#[cfg(test)]
fn preparation_cancellable(task: Option<&TaskToken>) -> Result<(Manifest, PathBuf)> {
    checkpoint(task)?;
    if !cfg!(target_arch = "x86_64") {
        return Err("本 OCR CPU 依赖目前仅支持 Windows x64".into());
    }
    check_visual_cpp_cancellable(task)?;
    let manifest = checked(task, manifest)?;
    let root = checked(task, || {
        Ok(std::path::absolute(crate::preferences::data_dir())?)
    })?
    .join("ocr")
    .join(&manifest.cache_revision);
    checked(task, || Ok(fs::create_dir_all(&root)?))?;
    Ok((manifest, root))
}

/// 发布包在 EXE 同目录放置 ONNX Runtime；与固定清单逐字节一致时直接使用，不再下载。
/// 开发版或文件不符时回到缓存下载路线。核对结果按进程缓存，避免每次识别重算哈希。
fn bundled_runtime_directory(
    manifest: &Manifest,
    task: Option<&TaskToken>,
) -> Result<Option<PathBuf>> {
    static BUNDLED: Mutex<Option<(String, Option<PathBuf>)>> = Mutex::new(None);
    let key = manifest.runtime.directory.clone();
    if let Some((cached_key, directory)) =
        BUNDLED.lock().map_err(|_| "OCR 运行库探测锁失效")?.as_ref()
    {
        if *cached_key == key {
            return Ok(directory.clone());
        }
    }
    let Some(directory) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .filter(|directory| directory.is_absolute())
    else {
        return Ok(None);
    };
    let mut complete = true;
    for file in &manifest.runtime.files {
        // 同名但内容不同的文件不是本包的运行库，回到下载路线而不是报错。
        let verified =
            match verified_cancellable(&directory.join(&file.name), file.bytes, &file.sha256, task)
            {
                Ok(verified) => verified,
                Err(_) => {
                    checkpoint(task)?;
                    false
                }
            };
        if !verified {
            complete = false;
            break;
        }
    }
    let found = complete.then_some(directory);
    *BUNDLED.lock().map_err(|_| "OCR 运行库探测锁失效")? = Some((key, found.clone()));
    Ok(found)
}

fn with_bundled_runtime(
    manifest: &Manifest,
    mut locations: CacheLocations,
    task: Option<&TaskToken>,
) -> Result<CacheLocations> {
    if let Some(directory) = bundled_runtime_directory(manifest, task)? {
        locations.runtime = directory.join("onnxruntime.dll");
        locations.bundled_runtime = true;
    }
    Ok(locations)
}

fn prepare_locations_runtime_cancellable(
    manifest: &Manifest,
    locations: &CacheLocations,
    task: Option<&TaskToken>,
) -> Result<PathBuf> {
    if locations.bundled_runtime {
        checkpoint(task)?;
        return Ok(locations.runtime.clone());
    }
    prepare_runtime_cancellable(manifest, &locations.runtime_root, task)
}

fn check_visual_cpp_cancellable(task: Option<&TaskToken>) -> Result<()> {
    checkpoint(task)?;
    let mut buffer = [0u16; 512];
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    checkpoint(task)?;
    if length == 0 || length >= buffer.len() {
        return Err("无法定位 Windows 系统运行库目录".into());
    }
    let system = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
    // 发布包在 EXE 同目录附带 app-local 运行库；ONNX Runtime 的导入按应用目录优先解析。
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let mut libraries = Vec::new();
    for name in [
        "MSVCP140.dll",
        "MSVCP140_1.dll",
        "VCRUNTIME140.dll",
        "VCRUNTIME140_1.dll",
    ] {
        // 精确 System32 或 EXE 目录路径，不从当前工作目录搜寻同名库；由系统负责 UCRT 导入。
        let library = checked(task, || {
            unsafe { libloading::Library::new(system.join(name)) }
                .or_else(|error| {
                    match bundled.as_ref().map(|directory| directory.join(name)) {
                        Some(local) if local.is_file() => unsafe { libloading::Library::new(local) },
                        _ => Err(error),
                    }
                })
                .map_err(|error| {
                    format!("OCR CPU 依赖缺少或无法加载 Microsoft Visual C++ x64 运行库 {name}：{error}。请完整解压 {} 发布包，或安装官方 2015–2022 x64 运行库：https://aka.ms/vs/17/release/vc_redist.x64.exe", crate::brand::NAME).into()
                })
        })?;
        libraries.push(library);
    }
    Ok(())
}

/// 旧清单下载的官方完整包（含约 382MB 解压体积的 PDB）；成员已解出后不再需要。
const LEGACY_RUNTIME_ARCHIVES: [&str; 1] = ["onnxruntime-win-x64-1.24.4.zip"];

fn prepare_runtime_cancellable(
    manifest: &Manifest,
    root: &Path,
    task: Option<&TaskToken>,
) -> Result<PathBuf> {
    checkpoint(task)?;
    let directory = root.join(&manifest.runtime.directory);
    checked(task, || Ok(fs::create_dir_all(&directory)?))?;
    let mut complete = true;
    for file in &manifest.runtime.files {
        complete &=
            verified_cancellable(&directory.join(&file.name), file.bytes, &file.sha256, task)?;
    }
    if !complete {
        let archive_path = download_cancellable(root, &manifest.runtime.archive, task)?;
        let archive_file = checked(task, || Ok(File::open(&archive_path)?))?;
        let mut archive = checked(task, || Ok(zip::ZipArchive::new(archive_file)?))?;
        // 仅解出清单中的四个成员；不用 archive.extract，压缩路径不能决定落盘位置。
        for file in &manifest.runtime.files {
            let destination = directory.join(&file.name);
            if verified_cancellable(&destination, file.bytes, &file.sha256, task)? {
                continue;
            }
            checkpoint(task)?;
            let member = archive.by_name(&file.member);
            checkpoint(task)?;
            let mut member = member?;
            if member.size() != file.bytes {
                return Err("OCR 原生库压缩成员大小不符".into());
            }
            let mut pending = Pending::new_cancellable(&destination, task)?;
            copy_checked_cancellable(
                &mut member,
                pending.file.as_mut().ok_or("OCR 暂存文件已关闭")?,
                file.bytes,
                &file.sha256,
                task,
            )?;
            pending.publish_cancellable(&destination, file.bytes, &file.sha256, task)?;
        }
    }
    // 成员均已校验落盘，压缩包只是下载中转；删除失败不影响识别，下次再清。
    let downloaded = manifest.runtime.archive.name.as_str();
    for name in LEGACY_RUNTIME_ARCHIVES.into_iter().chain([downloaded]) {
        let _ = fs::remove_file(root.join(name));
    }
    checkpoint(task)?;
    Ok(directory.join("onnxruntime.dll"))
}

fn download_cancellable(
    directory: &Path,
    asset: &Asset,
    task: Option<&TaskToken>,
) -> Result<PathBuf> {
    download_with_agent_cancellable(directory, asset, task, download_agent)
}

fn download_with_agent_cancellable(
    directory: &Path,
    asset: &Asset,
    task: Option<&TaskToken>,
    agent_factory: impl FnOnce() -> Result<ureq::Agent>,
) -> Result<PathBuf> {
    checkpoint(task)?;
    let sources = asset.sources();
    for (index, source) in sources.iter().enumerate() {
        if sources[..index].iter().any(|seen| seen.name == source.name) {
            continue;
        }
        let destination = directory.join(source.name);
        match verified_cancellable(&destination, source.bytes, source.sha256, task) {
            Ok(true) => return Ok(destination),
            Ok(false) => {}
            // 备用来源的同名旧文件（如旧版官方整包）不符时不阻断下载，由主来源继续。
            Err(_) if index > 0 => checkpoint(task)?,
            Err(error) => return Err(error),
        }
    }
    download_progress::begin(asset.bytes);
    let agent = checked(task, agent_factory)?;
    let mut first_error = None;
    for index in download_source::order(&agent, &sources, task)? {
        let source = &sources[index];
        match download_source_cancellable(&agent, directory, source, task) {
            Ok(path) => return Ok(path),
            Err(error) => {
                // 取消优先；其它失败换下一个来源，全部失败时报告首个来源的原因。
                checkpoint(task)?;
                download_source::mark_failed(source.url);
                first_error.get_or_insert(error);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| "OCR 资产没有可用的下载来源".into()))
}

fn download_source_cancellable(
    agent: &ureq::Agent,
    directory: &Path,
    source: &Source<'_>,
    task: Option<&TaskToken>,
) -> Result<PathBuf> {
    let destination = directory.join(source.name);
    download_progress::begin(source.bytes);
    let response = download_response_cancellable(agent, source, task)?;
    // 收到响应后本来源不重试正文、固定校验或落盘失败；仅本次流可创建／发布暂存文件。
    let mut pending = Pending::new_cancellable(&destination, task)?;
    copy_checked_cancellable(
        &mut download_progress::counting(response.into_reader()),
        pending.file.as_mut().ok_or("OCR 暂存文件已关闭")?,
        source.bytes,
        source.sha256,
        task,
    )?;
    pending.publish_cancellable(&destination, source.bytes, source.sha256, task)?;
    checkpoint(task)?;
    Ok(destination)
}

const DOWNLOAD_RETRY_BACKOFFS: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(250)];

fn download_response_cancellable(
    agent: &ureq::Agent,
    source: &Source<'_>,
    task: Option<&TaskToken>,
) -> Result<ureq::Response> {
    let mut backoffs = DOWNLOAD_RETRY_BACKOFFS.into_iter();
    loop {
        checkpoint(task)?;
        // 单次 call 的 connect／headers 不能即时打断；沿用原代理、TLS 与网络超时。
        // 本层最多 3 次 call；ureq 自身的连接池恢复及重定向仍属于单次 call。
        let response = agent.get(source.url).call();
        checkpoint(task)?;
        match response {
            Ok(response) => return Ok(response),
            Err(error) => {
                if download_request_is_transient(&error) {
                    if let Some(delay) = backoffs.next() {
                        wait_for_download_retry(delay, task)?;
                        continue;
                    }
                }
                return Err(download_failure(source.name, &error).into());
            }
        }
    }
}

fn download_request_is_transient(error: &ureq::Error) -> bool {
    transient_download_error(error, 0)
}

fn transient_download_error(error: &ureq::Error, depth: usize) -> bool {
    let dns = match error.kind() {
        ureq::ErrorKind::Dns => true,
        ureq::ErrorKind::ConnectionFailed | ureq::ErrorKind::Io => false,
        _ => return false,
    };
    // 默认 rustls 的证书／协议错误可能标为 ConnectionFailed；必须核对实际 io kind。
    let Some(source) = std::error::Error::source(error) else {
        return false;
    };
    let Some(io) = source.downcast_ref::<std::io::Error>() else {
        return false;
    };
    transient_download_io(io, dns, depth)
}

fn transient_download_io(error: &std::io::Error, dns: bool, depth: usize) -> bool {
    if depth >= 8 {
        return false;
    }
    // Windows WSATRY_AGAIN=11002；永久解析失败 11001／NotFound 不在白名单中。
    let temporary_dns = dns && error.raw_os_error() == Some(11002);
    if !temporary_dns
        && !matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::WouldBlock
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
        )
    {
        return false;
    }
    match error.get_ref() {
        None => true,
        Some(source) => {
            if let Some(io) = source.downcast_ref::<std::io::Error>() {
                return transient_download_io(io, false, depth + 1);
            }
            if let Some(transport) = source.downcast_ref::<ureq::Error>() {
                return transient_download_error(transport, depth + 1);
            }
            // ureq 的 headers EOF 使用短暂 io kind + 字符串叶子；不匹配错误文本。
            // 未知的嵌套错误不重试，默认 rustls 语义错误的 InvalidData 已被排除。
            source.source().is_none()
        }
    }
}

fn wait_for_download_retry(delay: Duration, task: Option<&TaskToken>) -> Result<()> {
    checkpoint(task)?;
    let deadline = Instant::now() + delay;
    loop {
        checkpoint(task)?;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        checkpoint(task)?;
        std::thread::sleep(remaining.min(Duration::from_millis(10)));
        checkpoint(task)?;
    }
}

/// 翻译等其它后台 HTTP 也复用同一代理选择。
pub(crate) fn download_agent() -> Result<ureq::Agent> {
    let mut builder = ureq::AgentBuilder::new()
        .try_proxy_from_env(true)
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .timeout(Duration::from_secs(600));
    // 与 ureq 的环境优先级一致；显式配置无效时不能悄悄退回直连。
    let mut environment: [Option<String>; 6] = Default::default();
    for (index, name) in [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .into_iter()
    .enumerate()
    {
        environment[index] = match std::env::var(name) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => return Err("OCR 环境代理编码无效；代理地址和凭据未记录".into()),
        };
    }
    if let Some(proxy) = environment_proxy(environment.each_ref().map(|value| value.as_deref()))? {
        builder = builder.proxy(proxy);
    } else if let Some(proxy) = windows_manual_proxy()? {
        builder = builder.proxy(proxy);
    }
    Ok(builder.build())
}

fn environment_proxy(values: [Option<&str>; 6]) -> Result<Option<ureq::Proxy>> {
    let Some(value) = values.into_iter().flatten().next() else {
        return Ok(None);
    };
    if value.trim().is_empty() || value.starts_with("socks") {
        return Err("OCR 环境代理无效或使用了未支持的 SOCKS 协议；请配置 HTTP 代理".into());
    }
    ureq::Proxy::new(value)
        .map(Some)
        .map_err(|_| "OCR 环境代理格式无效；代理地址和凭据未记录".into())
}

fn windows_manual_proxy() -> Result<Option<ureq::Proxy>> {
    let enabled = internet_setting("ProxyEnable", RRF_RT_REG_DWORD)?
        .and_then(|data| {
            data.get(..4)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
        })
        .unwrap_or(0);
    if enabled != 0 {
        let server = internet_setting_string("ProxyServer")?.unwrap_or_default();
        let address = manual_proxy_url(&server)?;
        return ureq::Proxy::new(address)
            .map(Some)
            .map_err(|_| "OCR Windows 手动代理格式无效；代理地址和凭据未记录".into());
    }
    if internet_setting_string("AutoConfigURL")?.is_some_and(|url| !url.trim().is_empty()) {
        return Err("OCR 下载暂不支持 Windows PAC 自动代理脚本；请配置 HTTP/HTTPS_PROXY 环境变量或 Windows 手动 HTTP 代理".into());
    }
    Ok(None)
}

fn internet_setting_string(name: &str) -> Result<Option<String>> {
    internet_setting(name, RRF_RT_REG_SZ)?
        .map(|bytes| {
            let wide: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .take_while(|&unit| unit != 0)
                .collect();
            String::from_utf16(&wide).map_err(|_| "OCR Windows 代理配置编码无效".into())
        })
        .transpose()
}

fn internet_setting(name: &str, flags: REG_ROUTINE_FLAGS) -> Result<Option<Vec<u8>>> {
    let key = HSTRING::from("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
    let name = HSTRING::from(name);
    let mut data = vec![0u8; 512];
    for _ in 0..3 {
        let mut size = data.len() as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                flags,
                None,
                Some(data.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
            return Ok(None);
        }
        if status == ERROR_MORE_DATA && size <= 65536 {
            data.resize(size as usize, 0);
            continue;
        }
        status
            .ok()
            .map_err(|_| "OCR 无法只读获取 Windows 代理配置")?;
        data.truncate(size as usize);
        return Ok(Some(data));
    }
    Err("OCR Windows 代理配置在读取期间持续变化".into())
}

fn manual_proxy_url(config: &str) -> Result<String> {
    let config = config.trim();
    let address = if config.contains('=') {
        let mut https = None;
        let mut http = None;
        for entry in config.split(';').filter(|entry| !entry.trim().is_empty()) {
            let (scheme, value) = entry
                .split_once('=')
                .ok_or("OCR Windows 代理映射格式无效")?;
            if scheme.trim().eq_ignore_ascii_case("https") && !value.trim().is_empty() {
                https = Some(value.trim());
            }
            if scheme.trim().eq_ignore_ascii_case("http") && !value.trim().is_empty() {
                http = Some(value.trim());
            }
        }
        https
            .or(http)
            .ok_or("OCR Windows 代理没有可用的 HTTP/HTTPS 映射")?
    } else {
        config
    };
    let authority = address.strip_prefix("http://").unwrap_or(address);
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or("OCR Windows 手动 HTTP 代理必须为 host:port")?;
    if host.is_empty()
        || !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
        || port.parse::<u16>().ok().is_none_or(|port| port == 0)
    {
        return Err("OCR Windows 手动 HTTP 代理地址无效；代理地址和凭据未记录".into());
    }
    Ok(format!("http://{host}:{port}"))
}

fn download_failure(asset: &str, error: &ureq::Error) -> String {
    // Error Display/source/URL 可包含代理用户和密码，仅保留无地址的分类。
    format!("OCR 依赖下载失败（{asset}）：{:?}；请检查网络、HTTP 代理或 TLS 连接。PAC 自动代理脚本暂不支持", error.kind())
}

fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
    {
        return Err("OCR 清单文件名必须是单个普通文件名".into());
    }
    Ok(())
}
fn valid_identity(bytes: u64, hash: &str) -> Result<()> {
    if bytes == 0 || hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("OCR 清单大小或 SHA256 无效".into());
    }
    Ok(())
}
#[cfg(test)]
fn verified(path: &Path, bytes: u64, expected: &str) -> Result<bool> {
    verified_cancellable(path, bytes, expected, None)
}

fn verified_cancellable(
    path: &Path,
    bytes: u64,
    expected: &str,
    task: Option<&TaskToken>,
) -> Result<bool> {
    if !checked(task, || Ok(path.try_exists()?))? {
        return Ok(false);
    }
    let metadata = checked(task, || Ok(fs::symlink_metadata(path)?))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != bytes {
        return Err(format!("OCR 已有依赖大小或类型不符，未覆盖：{}", path.display()).into());
    }
    let file = checked(task, || Ok(File::open(path)?))?;
    // 元数据与读取之间若被外部进程变更，也至多读取固定大小加一个字节。
    let limit = bytes.checked_add(1).ok_or("OCR 校验大小溢出")?;
    let (actual_bytes, actual_hash) = hash_reader_cancellable(&mut file.take(limit), task)?;
    if actual_bytes != bytes || !actual_hash.eq_ignore_ascii_case(expected) {
        return Err(format!("OCR 已有依赖 SHA256 不符，未覆盖：{}", path.display()).into());
    }
    checkpoint(task)?;
    Ok(true)
}

/// 被真实缓存校验消费的分块哈希；读取错误不保留来源地址或任意底层消息。
fn hash_reader_cancellable(
    reader: &mut impl Read,
    task: Option<&TaskToken>,
) -> Result<(u64, String)> {
    checkpoint(task)?;
    let mut total = 0u64;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        checkpoint(task)?;
        let count = reader.read(&mut buffer);
        checkpoint(task)?;
        let count = count.map_err(|_| "OCR 依赖数据流读取失败；网络地址和代理凭据未记录")?;
        if count == 0 {
            break;
        }
        total = total.checked_add(count as u64).ok_or("OCR 校验大小溢出")?;
        hash.update(&buffer[..count]);
        checkpoint(task)?;
    }
    let digest = format!("{:x}", hash.finalize());
    checkpoint(task)?;
    Ok((total, digest))
}
#[cfg(test)]
fn copy_checked(
    reader: &mut impl Read,
    writer: &mut impl Write,
    expected_bytes: u64,
    expected_hash: &str,
) -> Result<()> {
    copy_checked_cancellable(reader, writer, expected_bytes, expected_hash, None)
}

fn copy_checked_cancellable(
    reader: &mut impl Read,
    writer: &mut impl Write,
    expected_bytes: u64,
    expected_hash: &str,
    task: Option<&TaskToken>,
) -> Result<()> {
    checkpoint(task)?;
    let mut total = 0u64;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        checkpoint(task)?;
        let count = reader.read(&mut buffer);
        checkpoint(task)?;
        let count = count.map_err(|_| "OCR 依赖数据流读取失败；网络地址和代理凭据未记录")?;
        if count == 0 {
            break;
        }
        total = total.checked_add(count as u64).ok_or("OCR 下载大小溢出")?;
        if total > expected_bytes {
            return Err("OCR 依赖超出固定下载大小".into());
        }
        let mut remaining = &buffer[..count];
        while !remaining.is_empty() {
            checkpoint(task)?;
            let written = writer.write(remaining);
            checkpoint(task)?;
            match written {
                Ok(0) => return Err("OCR 依赖暂存写入没有进展".into()),
                Ok(written) if written <= remaining.len() => remaining = &remaining[written..],
                Ok(_) => return Err("OCR 依赖暂存写入长度无效".into()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err("OCR 依赖暂存写入失败".into()),
            }
        }
        hash.update(&buffer[..count]);
        checkpoint(task)?;
    }
    checkpoint(task)?;
    let actual_hash = format!("{:x}", hash.finalize());
    checkpoint(task)?;
    if total != expected_bytes || !actual_hash.eq_ignore_ascii_case(expected_hash) {
        return Err("OCR 下载或解压文件的大小/SHA256 校验失败".into());
    }
    checkpoint(task)?;
    Ok(())
}

struct Pending {
    path: PathBuf,
    file: Option<File>,
}
impl Pending {
    #[cfg(test)]
    fn new(destination: &Path) -> Result<Self> {
        Self::new_cancellable(destination, None)
    }

    fn new_cancellable(destination: &Path, task: Option<&TaskToken>) -> Result<Self> {
        for _ in 0..16 {
            checkpoint(task)?;
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path =
                destination.with_file_name(format!(".ocr-{}-{sequence}.part", std::process::id()));
            let opened = OpenOptions::new().write(true).create_new(true).open(&path);
            match opened {
                Ok(file) => {
                    let pending = Self {
                        path,
                        file: Some(file),
                    };
                    // 已建立 RAII 之后才传播取消，确保刚创建的 .part 也会清理。
                    checkpoint(task)?;
                    return Ok(pending);
                }
                Err(error) => {
                    checkpoint(task)?;
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        continue;
                    }
                    return Err(error.into());
                }
            }
        }
        Err("OCR 暂存文件名连续冲突".into())
    }
    #[cfg(test)]
    fn publish(self, destination: &Path, bytes: u64, hash: &str) -> Result<()> {
        self.publish_cancellable(destination, bytes, hash, None)
    }

    fn publish_cancellable(
        mut self,
        destination: &Path,
        bytes: u64,
        hash: &str,
        task: Option<&TaskToken>,
    ) -> Result<()> {
        checkpoint(task)?;
        let file = self.file.take().ok_or("OCR 暂存文件已关闭")?;
        checked(task, || Ok(file.sync_all()?))?;
        drop(file);
        let source: Vec<u16> = self.path.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // 不加 REPLACE_EXISTING；跨进程胜出的已验证文件可复用，不能覆盖已加载 DLL。
        checkpoint(task)?;
        let moved = unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
        };
        // 成功移动已是跨进程提交点。此后取消只返回取消，不撤销合法缓存。
        checkpoint(task)?;
        match moved {
            Ok(()) => Ok(()),
            Err(error) => {
                if verified_cancellable(destination, bytes, hash, task)? {
                    Ok(())
                } else {
                    Err(error.into())
                }
            }
        }
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.file.take();
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ABC_SHA: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    #[test]
    fn fixed_cpu_manifest_has_api24_and_exact_tiny_files() {
        let manifest = manifest().unwrap();
        assert_eq!(manifest.runtime_api, 24);
        assert_eq!(manifest.runtime.files.len(), 4);
        assert_eq!(manifest.models.detection.bytes, 1780590);
        assert_eq!(manifest.models.classification.name, "cls.onnx");
        assert_eq!(manifest.models.classification.bytes, 1019454);
        assert_eq!(
            manifest.models.classification.sha256,
            "94a6a0a0425f2b5f08b5df72086f2d72abe40f1d22f6d12d2cd83674f11f2ff3"
        );
        assert_eq!(manifest.models.recognition.bytes, 4462639);
        assert_eq!(manifest.models.dictionary.bytes, 27156);
        assert!(manifest.runtime.archive.url.contains("/v1.24.4/"));
        assert_eq!(manifest.runtime.archive.sources().len(), 1);
        for asset in manifest.models.recognition_assets() {
            assert_eq!(asset.sources().len(), 1);
        }
    }
    #[test]
    fn detection_preparation_excludes_classifier_recognizer_and_dictionary() {
        let manifest = manifest().unwrap();
        let names = manifest
            .models
            .detection_assets()
            .map(|asset| asset.name.as_str());
        assert_eq!(names, ["LICENSE-PaddleOCR.txt", "det.onnx"]);
        let names = manifest
            .models
            .recognition_assets()
            .map(|asset| asset.name.as_str());
        assert_eq!(
            names,
            [
                "LICENSE-PaddleOCR.txt",
                "det.onnx",
                "cls.onnx",
                "rec.onnx",
                "ppocrv6_tiny_dict.txt",
            ]
        );
    }
    #[test]
    fn checksum_copy_rejects_truncation_same_length_change_and_extra_bytes() {
        let mut bytes = Vec::new();
        copy_checked(&mut std::io::Cursor::new(b"abc"), &mut bytes, 3, ABC_SHA).unwrap();
        assert_eq!(bytes, b"abc");
        for input in [b"ab".as_slice(), b"abd".as_slice(), b"abcd".as_slice()] {
            assert!(copy_checked(
                &mut std::io::Cursor::new(input),
                &mut Vec::new(),
                3,
                ABC_SHA
            )
            .is_err());
        }
    }
    #[test]
    fn asset_name_rejects_absolute_paths_and_archive_traversal() {
        for name in [
            "../model.onnx",
            "..\\model.onnx",
            "C:\\model.onnx",
            "",
            ".",
            "..",
        ] {
            assert!(valid_name(name).is_err());
        }
        valid_name("onnxruntime_providers_shared.dll").unwrap();
    }
    #[test]
    fn windows_manual_proxy_selects_https_then_http_and_single_endpoint() {
        assert_eq!(
            manual_proxy_url("127.0.0.1:7890").unwrap(),
            "http://127.0.0.1:7890"
        );
        assert_eq!(
            manual_proxy_url(" http=first:8080 ; HTTPS=second:9090 ; socks=other:1 ").unwrap(),
            "http://second:9090"
        );
        assert_eq!(
            manual_proxy_url("https= ; http=http://fallback:8080").unwrap(),
            "http://fallback:8080"
        );
        for config in [
            "",
            "socks=proxy:1080",
            "https://proxy:8080",
            "host:0",
            "host:65536",
            "user:secret@host:80",
            "host:80/path",
        ] {
            assert!(manual_proxy_url(config).is_err());
        }
    }
    #[test]
    fn environment_proxy_precedes_windows_and_rejects_invalid_without_direct_fallback() {
        assert!(environment_proxy([None; 6]).unwrap().is_none());
        let chosen = environment_proxy([
            None,
            None,
            Some("http://secure:8080"),
            None,
            Some("invalid://user:secret@bad"),
            None,
        ])
        .unwrap()
        .unwrap();
        assert_eq!(chosen, ureq::Proxy::new("http://secure:8080").unwrap());
        for invalid in ["", "invalid://user:secret@bad", "socks5://proxy:1080"] {
            let message = environment_proxy([Some(invalid), None, None, None, None, None])
                .unwrap_err()
                .to_string();
            assert!(!message.contains("secret"));
            assert!(!message.contains(invalid) || invalid.is_empty());
        }
    }
    #[test]
    fn download_error_reports_kind_without_proxy_credentials() {
        let error = ureq::Proxy::new("invalid://user:secret@proxy:8080").unwrap_err();
        let message = download_failure("det.onnx", &error);
        assert!(message.contains("det.onnx"));
        assert!(message.contains("InvalidProxyUrl"));
        for sensitive in ["user", "secret", "proxy:8080"] {
            assert!(!message.contains(sensitive));
        }
    }
}

#[cfg(test)]
#[path = "assets_cancel_tests.rs"]
mod cancel_tests;

#[cfg(test)]
#[path = "assets_cache_tests.rs"]
mod cache_tests;

#[cfg(test)]
#[path = "assets_profile_tests.rs"]
mod profile_tests;

#[cfg(test)]
#[path = "assets_download_retry_tests.rs"]
mod download_retry_tests;

#[cfg(test)]
#[path = "assets_table_tests.rs"]
mod table_tests;
