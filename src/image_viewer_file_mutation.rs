//! 用户明确提交后的单文件操作；读入图片本身不授予清理资格。
//!
//! 重命名绑定已核对身份的源句柄和父目录句柄，原子地拒绝覆盖。
//! 回收依赖 Shell 的 FOFX_RECYCLEONDELETE 合同，不提供永久删除或路径式降级。
//! Shell 的 PreDelete 身份复核与随后提交不是文件系统 CAS；外部进程仍可能在
//! 回调返回后改变路径。PostDelete 的实际回收项和最终源路径观察分别回报。
//! execute自行创建专用线程；调用方应在后台等待它。真实Shell行为须经显式原生验收。
use crate::{file_snapshot, file_snapshot::FileSnapshot, image_collection};
use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    mem::{offset_of, size_of},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::AsRawHandle,
    },
    path::{Component, Path, PathBuf, Prefix},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
};
use windows::{
    core::{implement, Error, Ref, HRESULT, PCWSTR, PWSTR},
    Win32::{
        Foundation::{ERROR_CANCELLED, E_ABORT, HANDLE},
        Storage::FileSystem::{
            FileIdInfo, FileRenameInfo, GetDriveTypeW, GetFileInformationByHandle,
            GetFileInformationByHandleEx, SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
            DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
            FILE_RENAME_INFO, FILE_RENAME_INFO_0, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IBindCtx,
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
        },
        UI::Shell::{
            FileOperation, IFileOperation, IFileOperationProgressSink,
            IFileOperationProgressSink_Impl, IShellItem, SHCreateItemFromParsingName,
            COPYENGINE_E_CANCELLED, COPYENGINE_E_USER_CANCELLED, FOFX_ADDUNDORECORD,
            FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FOF_NOCONFIRMATION, FOF_NOERRORUI,
            FOF_NO_CONNECTED_ELEMENTS, FOF_SILENT, SIGDN_FILESYSPATH,
            TSF_DELETE_RECYCLE_IF_POSSIBLE,
        },
    },
};

const MAX_PATH_UNITS: usize = 32_767;
const MAX_LEAF_UNITS: usize = 255;
const UNAVAILABLE_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub(crate) path: PathBuf,
    pub(crate) expected: FileSnapshot,
    pub(crate) token: u64,
    pub(crate) cancel: Arc<AtomicBool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// 完整叶名，允许更改扩展名；本操作不转换图片格式。
    Rename {
        new_leaf: String,
    },
    Recycle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Renamed {
        path: PathBuf,
        /// 只有目标路径确实仍指向已操作对象时才提供。
        snapshot: Option<FileSnapshot>,
    },
    Recycled {
        recycle_path: Option<PathBuf>,
    },
    Unchanged,
    /// 没有确认成功；仍应读取 source_after，不能据此断言源未变化。
    Cancelled,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Observation {
    Matches,
    Missing,
    Different(FileSnapshot),
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub(crate) token: u64,
    pub(crate) source_path: PathBuf,
    pub(crate) expected: FileSnapshot,
    pub(crate) outcome: Outcome,
    pub(crate) source_after: Observation,
    pub(crate) late_cancel: bool,
    pub(crate) warning: Option<String>,
}

struct Completion {
    outcome: Outcome,
    warning: Option<String>,
}

enum Failure {
    Cancelled,
    Failed(String),
}
type Attempt<T> = Result<T, Failure>;

/// 仅操作请求中的单个普通本机文件，不扫描或清理其它文件。
/// 取消在原生提交前检查；已确认完成的回执不会被迟到取消改写。
pub(crate) fn execute(request: Request, action: Action) -> Receipt {
    if request.cancel.load(Ordering::Acquire) {
        return execute_inner(request, action);
    }
    let worker_request = request.clone();
    let spawned = std::thread::Builder::new()
        .name("image-file-mutation".into())
        .spawn(move || execute_inner(worker_request, action));
    match spawned {
        Ok(worker) => match worker.join() {
            Ok(receipt) => receipt,
            Err(_) => infrastructure_failure(
                request,
                tr!(
                    "文件操作线程异常结束，实际结果未确认",
                    "The file worker stopped unexpectedly; the outcome is unconfirmed"
                )
                .into(),
                true,
            ),
        },
        Err(error) => infrastructure_failure(
            request,
            format!(
                "{}: {error}",
                tr!("无法启动文件操作线程", "Could not start the file worker")
            ),
            false,
        ),
    }
}

fn infrastructure_failure(request: Request, message: String, uncertain: bool) -> Receipt {
    let source_after = observe(&request.path, &request.expected);
    Receipt {
        token: request.token,
        source_path: request.path,
        expected: request.expected,
        outcome: Outcome::Failed(message),
        source_after,
        late_cancel: false,
        warning: uncertain.then(|| {
            tr!(
                "请根据源路径实际状态确认；不会自动重试或清理文件",
                "Check the source state; no automatic retry or cleanup was attempted"
            )
            .into()
        }),
    }
}

fn execute_inner(request: Request, action: Action) -> Receipt {
    if request.cancel.load(Ordering::Acquire) {
        return Receipt {
            token: request.token,
            source_path: request.path,
            expected: request.expected,
            outcome: Outcome::Cancelled,
            source_after: Observation::Unavailable(
                tr!(
                    "操作已取消，未重新检查源",
                    "Cancelled before checking the source"
                )
                .into(),
            ),
            late_cancel: false,
            warning: None,
        };
    }
    let result = (|| {
        let path = local_absolute_path(&request.path)?;
        check_cancel(&request.cancel)?;
        match action {
            Action::Rename { new_leaf } => rename(&path, &new_leaf, &request),
            Action::Recycle => recycle(&path, &request),
        }
    })();
    let completion = match result {
        Ok(done) => done,
        Err(Failure::Cancelled) => Completion {
            outcome: Outcome::Cancelled,
            warning: None,
        },
        Err(Failure::Failed(error)) => Completion {
            outcome: Outcome::Failed(error),
            warning: None,
        },
    };
    let source_after = observe(&request.path, &request.expected);
    let late_cancel = completed(&completion.outcome) && request.cancel.load(Ordering::Acquire);
    Receipt {
        token: request.token,
        source_path: request.path,
        expected: request.expected,
        outcome: completion.outcome,
        source_after,
        late_cancel,
        warning: completion.warning,
    }
}

fn completed(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Renamed { .. } | Outcome::Recycled { .. })
}

fn failed(error: impl std::fmt::Display) -> Failure {
    Failure::Failed(error.to_string())
}

fn check_cancel(cancel: &AtomicBool) -> Attempt<()> {
    if cancel.load(Ordering::Acquire) {
        Err(Failure::Cancelled)
    } else {
        Ok(())
    }
}

fn checked<T, E: std::fmt::Display>(result: Result<T, E>, cancel: &AtomicBool) -> Attempt<T> {
    check_cancel(cancel)?;
    result.map_err(failed)
}

fn observe(path: &Path, expected: &FileSnapshot) -> Observation {
    if let Err(error) = image_collection::ensure_local_file_or_missing(path) {
        return Observation::Unavailable(error);
    }
    match file_snapshot::inspect(path) {
        Ok(Some(actual)) if actual == *expected => Observation::Matches,
        Ok(Some(actual)) => Observation::Different(actual),
        Ok(None) => Observation::Missing,
        Err(error) => Observation::Unavailable(error.to_string()),
    }
}

fn local_absolute_path(path: &Path) -> Attempt<PathBuf> {
    let path = std::path::absolute(path).map_err(failed)?;
    let mut components = path.components();
    let drive = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
            _ => {
                return Err(failed(tr!(
                    "只支持本机磁盘文件",
                    "Only local disk files are supported"
                )))
            }
        },
        _ => return Err(failed(tr!("文件路径无效", "Invalid file path"))),
    };
    if components.next() != Some(Component::RootDir)
        || path.file_name().is_none()
        || components.any(|component| match component {
            Component::Normal(name) => name
                .encode_wide()
                .any(|unit| unit == 0 || unit == b':' as u16),
            _ => true,
        })
        || path.as_os_str().encode_wide().count() >= MAX_PATH_UNITS
    {
        return Err(failed(tr!("文件路径无效", "Invalid file path")));
    }
    // 支持本机可移动盘/固定盘/内存盘；网络映射盘与非本地provider不取得资格。
    let root = [drive as u16, b':' as u16, b'\\' as u16, 0];
    if !local_drive_type(unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) }) {
        return Err(failed(tr!(
            "只支持本机磁盘文件",
            "Only local disk files are supported"
        )));
    }
    Ok(path)
}

fn local_drive_type(kind: u32) -> bool {
    // Win32 DRIVE_REMOVABLE/DRIVE_FIXED/DRIVE_RAMDISK。
    matches!(kind, 2 | 3 | 6)
}

fn validate_leaf(name: &str) -> Result<Vec<u16>, String> {
    if name.is_empty()
        || name.encode_utf16().take(MAX_LEAF_UNITS + 1).count() > MAX_LEAF_UNITS
        || matches!(name, "." | "..")
        || name.ends_with('.')
        || name.ends_with(' ')
        || name
            .chars()
            .any(|ch| ch <= '\u{1f}' || "<>:\"/\\|?*".contains(ch))
    {
        return Err(tr!(
            "请输入有效的文件名，不要包含路径或尾部点/空格",
            "Enter a valid leaf name without a path or trailing dot/space"
        )
        .into());
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    let device = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        upper.strip_prefix(*prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    if device {
        return Err(tr!("此名称是系统保留设备名", "This is a reserved device name").into());
    }
    Ok(name.encode_utf16().collect())
}

fn ensure_expected(path: &Path, expected: &FileSnapshot, cancel: &AtomicBool) -> Attempt<()> {
    checked(image_collection::ensure_local_file(path), cancel)?;
    let snapshot = checked(file_snapshot::inspect(path), cancel)?;
    if snapshot.as_ref() != Some(expected) {
        return Err(failed(tr!(
            "源文件已被替换或修改，请重新打开后再操作",
            "The source changed; reopen it before this operation"
        )));
    }
    Ok(())
}

fn open_source(path: &Path, expected: &FileSnapshot, cancel: &AtomicBool) -> Attempt<File> {
    checked(image_collection::ensure_local_file(path), cancel)?;
    // 禁止并发写/删，直到已绑定对象的重命名完成；回收不持有这个句柄。
    let file = checked(
        OpenOptions::new()
            .access_mode((DELETE | FILE_READ_ATTRIBUTES).0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path),
        cancel,
    )?;
    let actual = checked(file_snapshot::snapshot(&file), cancel)?;
    if actual != *expected {
        return Err(failed(tr!(
            "源文件已被替换或修改，请重新打开后再操作",
            "The source changed; reopen it before this operation"
        )));
    }
    Ok(file)
}

fn open_parent(path: &Path, expected: &FileSnapshot, cancel: &AtomicBool) -> Attempt<File> {
    checked(image_collection::ensure_local_directory(path), cancel)?;
    let directory = checked(
        OpenOptions::new()
            .access_mode((FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY | FILE_TRAVERSE).0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(path),
        cancel,
    )?;
    let handle = HANDLE(directory.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    checked(
        unsafe { GetFileInformationByHandle(handle, &mut info) },
        cancel,
    )?;
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || info.dwFileAttributes & UNAVAILABLE_ATTRIBUTES != 0
    {
        return Err(failed(tr!(
            "父目录不是已落地的普通目录",
            "The parent is not a local regular directory"
        )));
    }
    let mut identity = FILE_ID_INFO::default();
    checked(
        unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                (&mut identity as *mut FILE_ID_INFO).cast(),
                size_of::<FILE_ID_INFO>() as u32,
            )
        },
        cancel,
    )?;
    if identity.VolumeSerialNumber != expected.volume {
        return Err(failed(tr!(
            "源与父目录卷身份不一致",
            "The source and parent volume identities differ"
        )));
    }
    Ok(directory)
}

/// usize backing 提供 FILE_RENAME_INFO 所需对齐，尾随数组从真实字段偏移起算。
struct RenameInfo {
    words: Vec<usize>,
    bytes: u32,
}
impl RenameInfo {
    fn new(parent: HANDLE, name: &[u16]) -> Result<Self, String> {
        let name_bytes = name
            .len()
            .checked_mul(2)
            .ok_or_else(|| tr!("文件名过长", "File name too long").to_string())?;
        let bytes = offset_of!(FILE_RENAME_INFO, FileName)
            .checked_add(name_bytes)
            .map(|bytes| bytes.max(size_of::<FILE_RENAME_INFO>()))
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or_else(|| tr!("文件名过长", "File name too long").to_string())?;
        let count = (bytes as usize).div_ceil(size_of::<usize>());
        let mut words = vec![0usize; count];
        let info = words.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            // 逐字段写入，保留 backing 中显式归零的 union/struct padding。
            std::ptr::addr_of_mut!((*info).Anonymous).write(FILE_RENAME_INFO_0 { Flags: 0 });
            std::ptr::addr_of_mut!((*info).RootDirectory).write(parent);
            std::ptr::addr_of_mut!((*info).FileNameLength).write(name_bytes as u32);
            let tail = words
                .as_mut_ptr()
                .cast::<u8>()
                .add(offset_of!(FILE_RENAME_INFO, FileName))
                .cast::<u16>();
            std::ptr::copy_nonoverlapping(name.as_ptr(), tail, name.len());
        }
        Ok(Self { words, bytes })
    }
}

fn rename(path: &Path, new_leaf: &str, request: &Request) -> Attempt<Completion> {
    let units = validate_leaf(new_leaf).map_err(failed)?;
    let parent_path = path
        .parent()
        .ok_or_else(|| failed(tr!("缺少父目录", "Missing parent directory")))?;
    let destination = parent_path.join(new_leaf);
    if destination.as_os_str().encode_wide().count() >= MAX_PATH_UNITS {
        return Err(failed(tr!("目标路径过长", "Destination path too long")));
    }
    let parent = open_parent(parent_path, &request.expected, &request.cancel)?;
    let source = open_source(path, &request.expected, &request.cancel)?;
    ensure_expected(path, &request.expected, &request.cancel)?;
    if path.file_name() == Some(std::ffi::OsStr::new(new_leaf)) {
        return Ok(Completion {
            outcome: Outcome::Unchanged,
            warning: None,
        });
    }
    let info = RenameInfo::new(HANDLE(parent.as_raw_handle()), &units).map_err(failed)?;
    check_cancel(&request.cancel)?;
    let result = unsafe {
        SetFileInformationByHandle(
            HANDLE(source.as_raw_handle()),
            FileRenameInfo,
            info.words.as_ptr().cast(),
            info.bytes,
        )
    };
    // 原生成功就是提交点；以下观察失败只作warning，绝不再报未完成或取消。
    if let Err(error) = result {
        check_cancel(&request.cancel)?;
        return Err(failed(error));
    }
    let mut warning = None;
    let snapshot = match file_snapshot::snapshot(&source) {
        Ok(actual) => {
            match image_collection::ensure_local_file(&destination)
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    file_snapshot::inspect(&destination).map_err(|error| error.to_string())
                }) {
                Ok(Some(observed)) if observed == actual => Some(actual),
                _ => {
                    add_warning(
                        &mut warning,
                        tr!(
                            "重命名已完成，但目标路径身份未能再次确认",
                            "Renamed, but the destination identity could not be confirmed"
                        ),
                    );
                    None
                }
            }
        }
        Err(error) => {
            add_warning(
                &mut warning,
                &format!(
                    "{}: {error}",
                    tr!(
                        "重命名已完成，但无法读取新身份",
                        "Renamed, but the new identity is unavailable"
                    )
                ),
            );
            None
        }
    };
    Ok(Completion {
        outcome: Outcome::Renamed {
            path: destination,
            snapshot,
        },
        warning,
    })
}

fn add_warning(target: &mut Option<String>, text: &str) {
    match target {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(text);
        }
        None => *target = Some(text.to_string()),
    }
}

struct Sta;
impl Sta {
    fn initialize() -> Attempt<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }
            .ok()
            .map_err(failed)?;
        Ok(Self)
    }
}
impl Drop for Sta {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct Advice<'a> {
    operation: &'a IFileOperation,
    cookie: u32,
}
impl Drop for Advice<'_> {
    fn drop(&mut self) {
        let _ = unsafe { self.operation.Unadvise(self.cookie) };
    }
}

#[derive(Clone, Default)]
struct RecycleState {
    pre_checked: bool,
    rejected: Option<String>,
    cancelled: bool,
    post_result: Option<HRESULT>,
    recycled: bool,
    recycle_path: Option<PathBuf>,
    finish_result: Option<HRESULT>,
    warning: Option<String>,
}

fn locked(state: &Mutex<RecycleState>) -> MutexGuard<'_, RecycleState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn recycle(path: &Path, request: &Request) -> Attempt<Completion> {
    ensure_expected(path, &request.expected, &request.cancel)?;
    let _sta = Sta::initialize()?;
    check_cancel(&request.cancel)?;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let item: IShellItem = checked(
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>) },
        &request.cancel,
    )?;
    let operation: IFileOperation = checked(
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER) },
        &request.cancel,
    )?;
    checked(
        unsafe {
            operation.SetOperationFlags(
                FOFX_RECYCLEONDELETE
                    | FOFX_ADDUNDORECORD
                    | FOFX_EARLYFAILURE
                    | FOF_NOCONFIRMATION
                    | FOF_NOERRORUI
                    | FOF_NO_CONNECTED_ELEMENTS
                    | FOF_SILENT,
            )
        },
        &request.cancel,
    )?;
    let state = Arc::new(Mutex::new(RecycleState::default()));
    let sink: IFileOperationProgressSink = RecycleSink {
        path: path.to_path_buf(),
        expected: request.expected.clone(),
        cancel: request.cancel.clone(),
        state: state.clone(),
    }
    .into();
    let cookie = checked(unsafe { operation.Advise(&sink) }, &request.cancel)?;
    let _advice = Advice {
        operation: &operation,
        cookie,
    };
    // 只注册全局sink一次，避免同一回调被全局和per-item重复投递。
    checked(
        unsafe { operation.DeleteItem(&item, None::<&IFileOperationProgressSink>) },
        &request.cancel,
    )?;
    ensure_expected(path, &request.expected, &request.cancel)?;
    let performed = unsafe { operation.PerformOperations() };
    let aborted = unsafe { operation.GetAnyOperationsAborted() };
    let state = locked(&state).clone();
    Ok(recycle_completion(
        &state,
        performed.err().map(|error| error.code()),
        aborted
            .map(|value| value.as_bool())
            .map_err(|error| error.code()),
        request.cancel.load(Ordering::Acquire),
    ))
}

fn cancelled_hresult(code: HRESULT) -> bool {
    code == E_ABORT
        || code == HRESULT::from_win32(ERROR_CANCELLED.0)
        || code == COPYENGINE_E_CANCELLED
        || code == COPYENGINE_E_USER_CANCELLED
}

fn recycle_completion(
    state: &RecycleState,
    perform_error: Option<HRESULT>,
    aborted: Result<bool, HRESULT>,
    cancelled: bool,
) -> Completion {
    let mut warning = state.warning.clone();
    // PostDelete实际成功+非NULL回收项优先于Perform/Finish/迟到取消。
    if state.recycled && state.pre_checked {
        if perform_error.is_some()
            || aborted != Ok(false)
            || state.finish_result.is_some_and(|code| code.is_err())
        {
            add_warning(
                &mut warning,
                tr!(
                    "已收到实际回收项，但 Shell 后续返回异常/中止",
                    "The item was recycled, but Shell later reported an error/abort"
                ),
            );
        }
        return Completion {
            outcome: Outcome::Recycled {
                recycle_path: state.recycle_path.clone(),
            },
            warning,
        };
    }
    // 成功但没有回收项不能声称回收，也不允许fallback删除。
    if state.post_result.is_some_and(|code| code.is_ok()) {
        return Completion {
            outcome: Outcome::Failed(
                tr!(
                    "Shell 未返回实际回收项，无法确认已移入回收站",
                    "Shell returned no recycled item; recycling is unconfirmed"
                )
                .into(),
            ),
            warning,
        };
    }
    if let Some(error) = &state.rejected {
        return Completion {
            outcome: if state.cancelled {
                Outcome::Cancelled
            } else {
                Outcome::Failed(error.clone())
            },
            warning,
        };
    }
    let code = state
        .post_result
        .filter(|code| code.is_err())
        .or(perform_error)
        .or_else(|| aborted.err())
        .or_else(|| state.finish_result.filter(|code| code.is_err()));
    if state.cancelled || code.is_some_and(cancelled_hresult) || (cancelled && aborted == Ok(true))
    {
        return Completion {
            outcome: Outcome::Cancelled,
            warning,
        };
    }
    let message = if let Some(code) = code {
        format!(
            "{}: {}",
            tr!("移入回收站失败", "Could not recycle the file"),
            Error::from_hresult(code)
        )
    } else if aborted == Ok(true) {
        tr!(
            "Shell 中止了操作，未确认移入回收站",
            "Shell aborted; recycling is unconfirmed"
        )
        .into()
    } else {
        tr!(
            "Shell 未提供文件回收完成回执",
            "Shell did not confirm that the file was recycled"
        )
        .into()
    };
    Completion {
        outcome: Outcome::Failed(message),
        warning,
    }
}

struct TaskString(PWSTR);
impl Drop for TaskString {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0 .0.cast())) };
    }
}

fn shell_path(item: &IShellItem) -> Result<PathBuf, String> {
    let text = TaskString(
        unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.map_err(|error| error.to_string())?,
    );
    if text.0.is_null() {
        return Err(tr!("Shell 未返回文件路径", "Shell returned no file path").into());
    }
    // Shell分配的NUL终止字符串；限制转存长度，保证不产生无界副本。
    let mut length = 0;
    unsafe {
        while length < MAX_PATH_UNITS && *text.0 .0.add(length) != 0 {
            length += 1;
        }
        if length == MAX_PATH_UNITS {
            return Err(tr!("Shell 文件路径过长", "Shell file path too long").into());
        }
        Ok(PathBuf::from(OsString::from_wide(
            std::slice::from_raw_parts(text.0 .0, length),
        )))
    }
}

#[implement(IFileOperationProgressSink)]
struct RecycleSink {
    path: PathBuf,
    expected: FileSnapshot,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<RecycleState>>,
}
impl RecycleSink {
    fn reject(&self, message: String, cancelled: bool) -> windows::core::Result<()> {
        let mut state = locked(&self.state);
        if state.rejected.is_none() {
            state.rejected = Some(message);
        }
        state.cancelled |= cancelled;
        Err(Error::from_hresult(E_ABORT))
    }
    fn check_cancel(&self) -> windows::core::Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            self.reject(
                tr!("文件操作已取消", "File operation cancelled").into(),
                true,
            )
        } else {
            Ok(())
        }
    }
    fn unexpected(&self) -> windows::core::Result<()> {
        self.reject(
            tr!(
                "拒绝回收请求以外的 Shell 操作",
                "Rejected an unexpected Shell operation"
            )
            .into(),
            false,
        )
    }
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for RecycleSink_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        self.check_cancel()
    }
    fn FinishOperations(&self, result: HRESULT) -> windows::core::Result<()> {
        locked(&self.state).finish_result = Some(result);
        Ok(())
    }
    fn PreDeleteItem(&self, flags: u32, item: Ref<IShellItem>) -> windows::core::Result<()> {
        self.check_cancel()?;
        if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
            return self.reject(
                tr!(
                    "Shell 未请求回收，已拒绝删除",
                    "Shell did not request recycling; deletion refused"
                )
                .into(),
                false,
            );
        }
        if locked(&self.state).pre_checked {
            return self.unexpected();
        }
        let item = match item.as_ref() {
            Some(item) => item,
            None => {
                return self.reject(
                    tr!("Shell 未提供待回收项", "Shell supplied no item to recycle").into(),
                    false,
                )
            }
        };
        let result = (|| {
            ensure_expected(&self.path, &self.expected, &self.cancel)?;
            let callback_path = shell_path(item).map_err(failed)?;
            let callback_path = local_absolute_path(&callback_path)?;
            ensure_expected(&callback_path, &self.expected, &self.cancel)
        })();
        match result {
            Ok(()) => {
                locked(&self.state).pre_checked = true;
                self.check_cancel()
            }
            Err(Failure::Cancelled) => self.reject(
                tr!("文件操作已取消", "File operation cancelled").into(),
                true,
            ),
            Err(Failure::Failed(error)) => self.reject(error, false),
        }
    }
    fn PostDeleteItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        result: HRESULT,
        recycled: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        let path = if result.is_ok() {
            recycled.as_ref().map(shell_path)
        } else {
            None
        };
        let mut state = locked(&self.state);
        state.post_result = Some(result);
        if result.is_ok() && recycled.as_ref().is_some() && state.pre_checked {
            state.recycled = true;
            match path {
                Some(Ok(path)) => state.recycle_path = Some(path),
                _ => add_warning(
                    &mut state.warning,
                    tr!(
                        "已收到回收项，但无法取得其文件系统路径",
                        "The recycled item has no available filesystem path"
                    ),
                ),
            }
        }
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
        self.check_cancel()
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        self.check_cancel()
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        self.check_cancel()
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        self.check_cancel()
    }
    fn PreRenameItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
        self.unexpected()
    }
    fn PostRenameItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        self.unexpected()
    }
    fn PostMoveItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreCopyItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
    ) -> windows::core::Result<()> {
        self.unexpected()
    }
    fn PostCopyItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> windows::core::Result<()> {
        self.unexpected()
    }
    fn PostNewItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        _: &PCWSTR,
        _: &PCWSTR,
        _: u32,
        _: HRESULT,
        _: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::E_FAIL;

    fn expected() -> FileSnapshot {
        FileSnapshot {
            volume: 3,
            id: [7; 16],
            created: 11,
            written: 13,
            bytes: 17,
            attributes: 0x20,
        }
    }

    #[test]
    fn local_drive_kinds_include_removable_and_ram_but_reject_remote() {
        for kind in [2, 3, 6] {
            assert!(local_drive_type(kind));
        }
        // UNKNOWN、NO_ROOT_DIR、REMOTE、CDROM以及未来未识别值均拒绝。
        for kind in [0, 1, 4, 5, 7, u32::MAX] {
            assert!(!local_drive_type(kind));
        }
    }

    #[test]
    fn leaf_rejects_paths_aliases_devices_and_win32_trimming() {
        for name in [
            "",
            ".",
            "..",
            "../x.png",
            "a/b.png",
            "a\\b.png",
            "C:x.png",
            "x.png:stream",
            "x\0.png",
            "x\n.png",
            "x?.png",
            "x*.png",
            "x<.png",
            "x>.png",
            "x|.png",
            "x\".png",
            "x.png.",
            "x.png ",
            "CON",
            "nul.jpg",
            "COM1.png",
            "com9 .png",
            "LPT1",
            "COM¹.png",
            "Lpt².jpg",
            "LPT³",
            "CONIN$",
            "conout$.png",
        ] {
            assert!(validate_leaf(name).is_err(), "{name:?}");
        }
        for name in [
            "photo.png",
            "photo.webp",
            ".png",
            "hello world.jpg",
            "副本 🖼.png",
            "COM10.png",
            "LPT0.png",
            "normal..png",
        ] {
            assert!(validate_leaf(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn name_limit_counts_utf16_and_extension_is_not_locked() {
        assert_eq!(validate_leaf(&"界".repeat(255)).unwrap().len(), 255);
        assert!(validate_leaf(&"界".repeat(256)).is_err());
        assert!(validate_leaf(&"🖼".repeat(128)).is_err());
        assert_eq!(
            validate_leaf(&format!("{}a", "🖼".repeat(127)))
                .unwrap()
                .len(),
            255
        );
        assert!(validate_leaf("previous.png.webp").is_ok());
        assert!(validate_leaf("extensionless").is_ok());
    }

    #[test]
    fn rename_buffer_uses_real_tail_offset_and_never_replace() {
        let name: Vec<_> = "新 🚀.webp".encode_utf16().collect();
        let parent = HANDLE(0x1234usize as *mut std::ffi::c_void);
        let buffer = RenameInfo::new(parent, &name).unwrap();
        assert_eq!(
            buffer.words.as_ptr() as usize % std::mem::align_of::<FILE_RENAME_INFO>(),
            0
        );
        let info = unsafe { &*buffer.words.as_ptr().cast::<FILE_RENAME_INFO>() };
        assert!(!unsafe { info.Anonymous.ReplaceIfExists });
        assert_eq!(info.RootDirectory, parent);
        assert_eq!(info.FileNameLength as usize, name.len() * 2);
        let tail = unsafe {
            std::slice::from_raw_parts(
                buffer
                    .words
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset_of!(FILE_RENAME_INFO, FileName))
                    .cast::<u16>(),
                name.len(),
            )
        };
        assert_eq!(tail, name.as_slice());
        assert_eq!(
            buffer.bytes as usize,
            size_of::<FILE_RENAME_INFO>()
                .max(offset_of!(FILE_RENAME_INFO, FileName) + name.len() * 2)
        );
    }

    #[test]
    fn pre_cancel_does_not_validate_or_observe_a_source_path() {
        let request = Request {
            path: PathBuf::from(r"\\server\not-accessed\x.png"),
            expected: expected(),
            token: 42,
            cancel: Arc::new(AtomicBool::new(true)),
        };
        let receipt = execute(
            request.clone(),
            Action::Rename {
                new_leaf: "../invalid".into(),
            },
        );
        assert_eq!(receipt.token, 42);
        assert_eq!(receipt.expected, request.expected);
        assert_eq!(receipt.outcome, Outcome::Cancelled);
        assert!(matches!(receipt.source_after, Observation::Unavailable(_)));
        assert!(!receipt.late_cancel);
    }

    #[test]
    fn cancellation_wins_over_an_uncommitted_read_error() {
        let cancel = AtomicBool::new(true);
        assert!(matches!(
            checked::<(), _>(Err("I/O failed"), &cancel),
            Err(Failure::Cancelled)
        ));
        cancel.store(false, Ordering::Release);
        assert!(matches!(
            checked::<(), _>(Err("I/O failed"), &cancel),
            Err(Failure::Failed(_))
        ));
    }

    #[test]
    fn queued_or_aborted_shell_calls_are_not_recycle_success() {
        let empty = RecycleState::default();
        assert!(matches!(
            recycle_completion(&empty, None, Ok(false), false).outcome,
            Outcome::Failed(_)
        ));
        assert!(matches!(
            recycle_completion(&empty, None, Ok(true), false).outcome,
            Outcome::Failed(_)
        ));
        assert_eq!(
            recycle_completion(&empty, Some(COPYENGINE_E_USER_CANCELLED), Ok(true), true).outcome,
            Outcome::Cancelled
        );
        let no_bin_item = RecycleState {
            pre_checked: true,
            post_result: Some(HRESULT(0)),
            ..Default::default()
        };
        assert!(matches!(
            recycle_completion(&no_bin_item, None, Ok(false), true).outcome,
            Outcome::Failed(_)
        ));
        let bad_source = RecycleState {
            rejected: Some("source changed".into()),
            ..Default::default()
        };
        assert_eq!(
            recycle_completion(&bad_source, Some(E_ABORT), Ok(true), false).outcome,
            Outcome::Failed("source changed".into())
        );
    }

    #[test]
    fn actual_recycled_item_survives_footer_error_and_late_cancel() {
        let path = PathBuf::from(r"C:\$Recycle.Bin\item");
        let state = RecycleState {
            pre_checked: true,
            post_result: Some(HRESULT(0)),
            recycled: true,
            recycle_path: Some(path.clone()),
            finish_result: Some(E_FAIL),
            cancelled: true,
            rejected: Some("late cancellation".into()),
            ..Default::default()
        };
        let receipt = recycle_completion(&state, Some(E_ABORT), Ok(true), true);
        assert_eq!(
            receipt.outcome,
            Outcome::Recycled {
                recycle_path: Some(path)
            }
        );
        assert!(receipt.warning.is_some());
        assert!(completed(&receipt.outcome));
        assert!(!completed(&Outcome::Cancelled));
        assert!(!completed(&Outcome::Unchanged));
    }

    #[test]
    fn actual_recycled_item_does_not_require_a_filesystem_display_path() {
        let state = RecycleState {
            pre_checked: true,
            post_result: Some(HRESULT(0)),
            recycled: true,
            recycle_path: None,
            ..Default::default()
        };
        assert_eq!(
            recycle_completion(&state, None, Ok(false), false).outcome,
            Outcome::Recycled { recycle_path: None }
        );
        let unqualified = RecycleState {
            pre_checked: false,
            ..state
        };
        assert!(matches!(
            recycle_completion(&unqualified, None, Ok(false), false).outcome,
            Outcome::Failed(_)
        ));
    }

    /// 仅供显式原生验收，文件均由本测试在独立临时目录创建。
    struct NativeFixture {
        root: PathBuf,
        temp: PathBuf,
    }
    impl NativeFixture {
        fn new() -> Self {
            let temp = local_absolute_path(&std::env::temp_dir())
                .unwrap_or_else(|_| panic!("Native fixtures require a local temporary directory"));
            assert!(
                !temp.components().any(|component| match component {
                    Component::Normal(name) => {
                        let name = name.to_string_lossy().to_ascii_lowercase();
                        name.starts_with("onedrive")
                            || name == "dropbox"
                            || name.starts_with("icloud")
                    }
                    _ => false,
                }),
                "Native fixtures must not use a cloud synchronization directory"
            );
            image_collection::ensure_local_directory(&temp).unwrap();
            let temp = std::fs::canonicalize(temp).unwrap();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = temp.join(format!(
                "rustshot-file-mutation-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir(&root).unwrap();
            let fixture = Self { root, temp };
            image_collection::ensure_local_directory(&fixture.root).unwrap();
            fixture.write_new("loaded.png", b"fixture pixels");
            fixture
        }
        fn write_new(&self, leaf: &str, bytes: &[u8]) {
            assert!(matches!(leaf, "loaded.png" | "existing.png"));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.root.join(leaf))
                .unwrap();
            std::io::Write::write_all(&mut file, bytes).unwrap();
        }
        fn cleanup(&self) -> std::io::Result<()> {
            image_collection::ensure_local_directory(&self.temp).map_err(std::io::Error::other)?;
            match std::fs::symlink_metadata(&self.root) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
            image_collection::ensure_local_directory(&self.root).map_err(std::io::Error::other)?;
            let actual = std::fs::canonicalize(&self.root)?;
            if actual.parent() != Some(self.temp.as_path())
                || actual.file_name() != self.root.file_name()
            {
                return Err(std::io::Error::other(
                    "Refusing fixture cleanup outside its private temporary directory",
                ));
            }
            // 固定自己创建的叶名；不递归，也不属于生产回收fallback。
            for name in [
                "loaded.png",
                "existing.png",
                "renamed.webp",
                "held-original.png",
            ] {
                match std::fs::remove_file(actual.join(name)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            std::fs::remove_dir(actual)
        }
        fn request(&self) -> Request {
            let path = self.root.join("loaded.png");
            Request {
                expected: file_snapshot::inspect(&path).unwrap().unwrap(),
                path,
                token: 71,
                cancel: Arc::new(AtomicBool::new(false)),
            }
        }
    }
    impl Drop for NativeFixture {
        fn drop(&mut self) {
            if let Err(error) = self.cleanup() {
                eprintln!("Native mutation fixture cleanup failed: {error}");
            }
        }
    }

    #[test]
    #[ignore = "显式原生验收：真实句柄重命名，默认不运行"]
    fn native_rename_is_bound_no_replace_and_does_not_convert_bytes() {
        let fixture = NativeFixture::new();
        let request = fixture.request();
        fixture.write_new("existing.png", b"keep destination");
        let denied = execute(
            request.clone(),
            Action::Rename {
                new_leaf: "existing.png".into(),
            },
        );
        assert!(matches!(denied.outcome, Outcome::Failed(_)), "{denied:?}");
        assert_eq!(denied.source_after, Observation::Matches);
        assert_eq!(
            std::fs::read(fixture.root.join("existing.png")).unwrap(),
            b"keep destination"
        );
        let done = execute(
            request.clone(),
            Action::Rename {
                new_leaf: "renamed.webp".into(),
            },
        );
        let Outcome::Renamed {
            path,
            snapshot: Some(actual),
        } = &done.outcome
        else {
            panic!("{done:?}")
        };
        assert_eq!(path, &fixture.root.join("renamed.webp"));
        assert_eq!(actual.id, request.expected.id);
        assert_eq!(actual.volume, request.expected.volume);
        assert_eq!(done.source_after, Observation::Missing);
        assert_eq!(std::fs::read(path).unwrap(), b"fixture pixels");
    }

    #[test]
    #[ignore = "显式原生验收：同长同mtime替换必须拒绝，默认不运行"]
    fn native_replaced_source_is_not_granted_mutation_eligibility() {
        let fixture = NativeFixture::new();
        let request = fixture.request();
        let modified = std::fs::metadata(&request.path)
            .unwrap()
            .modified()
            .unwrap();
        std::fs::rename(&request.path, fixture.root.join("held-original.png")).unwrap();
        fixture.write_new("loaded.png", b"fixture pixels");
        let replacement = OpenOptions::new().write(true).open(&request.path).unwrap();
        replacement
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        drop(replacement);
        let actual = file_snapshot::inspect(&request.path).unwrap().unwrap();
        assert_eq!(actual.bytes, request.expected.bytes);
        assert_eq!(actual.written, request.expected.written);
        assert_ne!(actual.id, request.expected.id);
        let denied = execute(
            request,
            Action::Rename {
                new_leaf: "renamed.webp".into(),
            },
        );
        assert!(matches!(denied.outcome, Outcome::Failed(_)), "{denied:?}");
        assert_eq!(denied.source_after, Observation::Different(actual));
        assert!(!fixture.root.join("renamed.webp").exists());
    }

    #[test]
    #[ignore = "显式原生验收：真实Shell回收测试自建文件，默认不运行"]
    fn native_recycle_requires_an_actual_bin_item_receipt() {
        let fixture = NativeFixture::new();
        let done = execute(fixture.request(), Action::Recycle);
        if !matches!(done.outcome, Outcome::Recycled { .. }) {
            // 失败时留存自建源供检查，不能以测试清理伪装成回收成功。
            let root = fixture.root.clone();
            std::mem::forget(fixture);
            panic!("{done:?}; fixture preserved: {}", root.display());
        }
        assert_eq!(done.source_after, Observation::Missing);
    }
}
