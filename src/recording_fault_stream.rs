#![cfg(test)]

//! 真实 MF 存储故障试验的文件流。只限制自有文件的字节长度，不伪造编码或收尾结果。
//! extent 上限不代表文件系统分配空间、元数据或整个进程内存的硬预算。
//! 所有产物保留；这里的人工空间不足不能证明系统磁盘真的已经耗尽。

#[path = "recording_byte_stream_fault_native.rs"]
mod byte_stream_native;

use std::{
    ffi::c_void,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::fs::OpenOptionsExt,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

use serde::Serialize;
use windows::{
    core::{implement, Error, Ref, HRESULT},
    Win32::{
        Foundation::{
            E_FAIL, E_NOTIMPL, E_POINTER, E_UNEXPECTED, STG_E_INVALIDFLAG, STG_E_INVALIDFUNCTION,
            STG_E_MEDIUMFULL, S_FALSE, S_OK,
        },
        Media::MediaFoundation::{IMFByteStream, MFCreateMFByteStreamOnStream},
        Storage::FileSystem::FILE_SHARE_READ,
        System::Com::{
            ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE, STATFLAG, STATFLAG_DEFAULT,
            STATFLAG_NONAME, STATFLAG_NOOPEN, STATSTG, STGC, STGM_READWRITE, STGM_SHARE_DENY_WRITE,
            STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
        },
    },
};

const SAFETY_CAP: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub(super) struct FaultSnapshot {
    pub actual_len: u64,
    pub high_water: u64,
    pub limit: u64,
    pub safety_cap: u64,
    pub armed: bool,
    pub arm_calls: u64,
    pub read_calls: u64,
    pub write_calls: u64,
    pub set_size_calls: u64,
    pub seek_calls: u64,
    pub commit_calls: u64,
    pub clone_calls: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub cap_hits: u64,
    pub injected_count: u64,
    pub injected_write_count: u64,
    pub injected_set_size_count: u64,
    pub first_injected_hresult: Option<u32>,
    pub last_error_hresult: Option<u32>,
}

impl Default for FaultSnapshot {
    fn default() -> Self {
        Self {
            actual_len: 0,
            high_water: 0,
            limit: SAFETY_CAP,
            safety_cap: SAFETY_CAP,
            armed: false,
            arm_calls: 0,
            read_calls: 0,
            write_calls: 0,
            set_size_calls: 0,
            seek_calls: 0,
            commit_calls: 0,
            clone_calls: 0,
            bytes_read: 0,
            bytes_written: 0,
            cap_hits: 0,
            injected_count: 0,
            injected_write_count: 0,
            injected_set_size_count: 0,
            first_injected_hresult: None,
            last_error_hresult: None,
        }
    }
}

struct FileState {
    file: File,
    stats: FaultSnapshot,
}

impl FileState {
    fn refresh_len(&mut self) -> windows::core::Result<()> {
        let metadata = self.file.metadata().map_err(|error| self.io_error(error))?;
        self.stats.actual_len = metadata.len();
        self.stats.high_water = self.stats.high_water.max(metadata.len());
        Ok(())
    }

    fn arm_limit(&mut self, limit: u64) -> crate::Result<()> {
        if limit < self.stats.actual_len || limit > SAFETY_CAP {
            return Err(tr!(
                "故障流限额必须不小于现有文件长度且不超过 4 MiB",
                "Fault stream limit must cover the existing file and not exceed 4 MiB"
            )
            .into());
        }
        self.stats.limit = limit;
        self.stats.armed = true;
        self.stats.arm_calls = self.stats.arm_calls.saturating_add(1);
        Ok(())
    }

    fn io_error(&mut self, error: std::io::Error) -> Error {
        // HRESULT_FROM_WIN32，保留操作系统原始低位错误码；不依赖错误文案分类。
        let code = match error.raw_os_error() {
            Some(code) if code > 0 => HRESULT((0x8007_0000 | (code as u32 & 0xffff)) as i32),
            Some(code) if code < 0 => HRESULT(code),
            _ => E_FAIL,
        };
        self.stats.last_error_hresult = Some(code.0 as u32);
        Error::from_hresult(code)
    }

    fn reject_extent(&mut self, write: bool) -> Error {
        self.stats.cap_hits = self.stats.cap_hits.saturating_add(1);
        self.stats.last_error_hresult = Some(STG_E_MEDIUMFULL.0 as u32);
        if self.stats.armed {
            self.stats.injected_count = self.stats.injected_count.saturating_add(1);
            let count = if write {
                &mut self.stats.injected_write_count
            } else {
                &mut self.stats.injected_set_size_count
            };
            *count = count.saturating_add(1);
            self.stats
                .first_injected_hresult
                .get_or_insert(STG_E_MEDIUMFULL.0 as u32);
        }
        Error::from_hresult(STG_E_MEDIUMFULL)
    }
}

#[derive(Clone)]
pub(super) struct FaultControl {
    shared: Arc<Mutex<FileState>>,
}

impl FaultControl {
    pub(super) fn snapshot(&self) -> crate::Result<FaultSnapshot> {
        let mut state = lock(&self.shared)?;
        state.refresh_len()?;
        Ok(state.stats.clone())
    }

    pub(super) fn arm(&self, limit: u64) -> crate::Result<()> {
        let mut state = lock(&self.shared)?;
        state.refresh_len()?;
        state.arm_limit(limit)
    }

    /// 长度读取与设限共用写入锁，避免 MF 后台写入在 snapshot→arm 间增长。
    pub(super) fn arm_after_current_len(&self, slack: u64) -> crate::Result<u64> {
        if slack == 0 {
            return Err(tr!(
                "故障流新增空间必须大于零",
                "Fault stream additional capacity must be greater than zero"
            )
            .into());
        }
        let mut state = lock(&self.shared)?;
        state.refresh_len()?;
        let limit = state.stats.actual_len.checked_add(slack).ok_or(tr!(
            "故障流限额计算溢出",
            "Fault stream limit calculation overflowed"
        ))?;
        state.arm_limit(limit)?;
        Ok(limit)
    }
}

/// 调用者已持有 MfRuntime；适配失败也只关闭句柄，不删除新建文件。
pub(super) fn create(path: &Path) -> crate::Result<(IMFByteStream, FaultControl)> {
    let (stream, control) = create_stream(path)?;
    let byte_stream = unsafe { MFCreateMFByteStreamOnStream(&stream) }?;
    Ok((byte_stream, control))
}

fn create_stream(path: &Path) -> crate::Result<(IStream, FaultControl)> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)?;
    let shared = Arc::new(Mutex::new(FileState {
        file,
        stats: FaultSnapshot::default(),
    }));
    let stream: IStream = FaultStream {
        shared: Arc::clone(&shared),
        cursor: Mutex::new(0),
    }
    .into();
    Ok((stream, FaultControl { shared }))
}

fn lock<T>(mutex: &Mutex<T>) -> windows::core::Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| Error::from_hresult(E_UNEXPECTED))
}

fn guarded<T>(operation: impl FnOnce() -> windows::core::Result<T>) -> windows::core::Result<T> {
    catch_unwind(AssertUnwindSafe(operation))
        .unwrap_or_else(|_| Err(Error::from_hresult(E_UNEXPECTED)))
}

// 所有带游标的操作固定先锁本流 cursor，再锁共享 file；Control 只锁 file。
// 每次 I/O 都在共享锁内恢复本流的游标，Clone 不共享文件句柄的隐含 seek 状态。
#[implement(IStream)]
struct FaultStream {
    shared: Arc<Mutex<FileState>>,
    cursor: Mutex<u64>,
}

impl FaultStream {
    fn read_buffer(
        &self,
        pv: *mut c_void,
        cb: u32,
        pcbread: *mut u32,
    ) -> windows::core::Result<HRESULT> {
        unsafe { store_count(pcbread, 0) };
        if pv.is_null() {
            return Err(Error::from_hresult(E_POINTER));
        }
        let mut cursor = lock(&self.cursor)?;
        let mut state = lock(&self.shared)?;
        state.stats.read_calls = state.stats.read_calls.saturating_add(1);
        if cb == 0 {
            return Ok(S_OK);
        }
        let available = state.stats.actual_len.saturating_sub(*cursor);
        let count = u64::from(cb).min(available) as usize;
        if count == 0 {
            return Ok(S_FALSE);
        }
        state
            .file
            .seek(SeekFrom::Start(*cursor))
            .map_err(|error| state.io_error(error))?;
        // 最多访问真实文件内的 4 MiB；COM 调用者保证非空缓冲区在 cb 范围内有效。
        let bytes = unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>(), count) };
        let mut read = 0;
        let mut failure = None;
        while read < count {
            match state.file.read(&mut bytes[read..]) {
                Ok(0) => break,
                Ok(amount) => read += amount,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    failure = Some(state.io_error(error));
                    break;
                }
            }
        }
        *cursor += read as u64;
        state.stats.bytes_read = state.stats.bytes_read.saturating_add(read as u64);
        unsafe { store_count(pcbread, read as u32) };
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(if read == cb as usize { S_OK } else { S_FALSE })
    }

    fn write_buffer(
        &self,
        pv: *const c_void,
        cb: u32,
        pcbwritten: *mut u32,
    ) -> windows::core::Result<HRESULT> {
        unsafe { store_count(pcbwritten, 0) };
        if pv.is_null() {
            return Err(Error::from_hresult(E_POINTER));
        }
        let mut cursor = lock(&self.cursor)?;
        let mut state = lock(&self.shared)?;
        state.stats.write_calls = state.stats.write_calls.saturating_add(1);
        if cb == 0 {
            return Ok(S_OK);
        }
        if *cursor > state.stats.limit || u64::from(cb) > state.stats.limit - *cursor {
            // 在创建输入 slice 或改文件之前验证整个请求；超额不做部分写入。
            return Err(state.reject_extent(true));
        }
        state
            .file
            .seek(SeekFrom::Start(*cursor))
            .map_err(|error| state.io_error(error))?;
        let bytes = unsafe { std::slice::from_raw_parts(pv.cast::<u8>(), cb as usize) };
        let mut written = 0;
        let mut failure = None;
        while written < bytes.len() {
            match state.file.write(&bytes[written..]) {
                Ok(0) => {
                    failure =
                        Some(state.io_error(std::io::Error::from(std::io::ErrorKind::WriteZero)));
                    break;
                }
                Ok(amount) => written += amount,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    failure = Some(state.io_error(error));
                    break;
                }
            }
        }
        if written != 0 {
            *cursor += written as u64;
            state.stats.actual_len = state.stats.actual_len.max(*cursor);
            state.stats.high_water = state.stats.high_water.max(state.stats.actual_len);
        }
        state.stats.bytes_written = state.stats.bytes_written.saturating_add(written as u64);
        unsafe { store_count(pcbwritten, written as u32) };
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(S_OK)
    }
}

// 输出计数指针可为空；非空指针的可写性和对齐由 COM 调用契约保证。
unsafe fn store_count<T: Copy>(target: *mut T, value: T) {
    if !target.is_null() {
        unsafe { target.write(value) };
    }
}

#[allow(non_snake_case)]
impl ISequentialStream_Impl for FaultStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        guarded(|| self.read_buffer(pv, cb, pcbread)).unwrap_or_else(|error| error.code())
    }

    fn Write(&self, pv: *const c_void, cb: u32, pcbwritten: *mut u32) -> HRESULT {
        guarded(|| self.write_buffer(pv, cb, pcbwritten)).unwrap_or_else(|error| error.code())
    }
}

#[allow(non_snake_case)]
impl IStream_Impl for FaultStream_Impl {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        guarded(|| {
            let mut cursor = lock(&self.cursor)?;
            let mut state = lock(&self.shared)?;
            state.stats.seek_calls = state.stats.seek_calls.saturating_add(1);
            let position = if dworigin == STREAM_SEEK_SET {
                // IStream 将 SET 的 LARGE_INTEGER 位模式解释为无符号绝对位置。
                Some(dlibmove as u64)
            } else {
                let base = if dworigin == STREAM_SEEK_CUR {
                    *cursor
                } else if dworigin == STREAM_SEEK_END {
                    state.stats.actual_len
                } else {
                    return Err(Error::from_hresult(STG_E_INVALIDFUNCTION));
                };
                if dlibmove >= 0 {
                    base.checked_add(dlibmove as u64)
                } else {
                    base.checked_sub(dlibmove.unsigned_abs())
                }
            }
            .ok_or_else(|| Error::from_hresult(STG_E_INVALIDFUNCTION))?;
            // Seek 自身不扩文件，也不因越过 extent 上限人工报空间不足。
            *cursor = position;
            unsafe { store_count(plibnewposition, position) };
            Ok(())
        })
    }

    fn SetSize(&self, libnewsize: u64) -> windows::core::Result<()> {
        guarded(|| {
            let mut state = lock(&self.shared)?;
            state.stats.set_size_calls = state.stats.set_size_calls.saturating_add(1);
            if libnewsize > state.stats.limit {
                return Err(state.reject_extent(false));
            }
            state
                .file
                .set_len(libnewsize)
                .map_err(|error| state.io_error(error))?;
            state.stats.actual_len = libnewsize;
            state.stats.high_water = state.stats.high_water.max(libnewsize);
            // IStream::SetSize 不改变任一 Clone 的独立游标。
            Ok(())
        })
    }

    fn CopyTo(
        &self,
        _: Ref<'_, IStream>,
        _: u64,
        pcbread: *mut u64,
        pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        guarded(|| {
            unsafe {
                store_count(pcbread, 0);
                store_count(pcbwritten, 0);
            }
            Err(Error::from_hresult(E_NOTIMPL))
        })
    }

    fn Commit(&self, grfcommitflags: &STGC) -> windows::core::Result<()> {
        guarded(|| {
            if grfcommitflags.0 & !0x0f != 0 {
                return Err(Error::from_hresult(STG_E_INVALIDFLAG));
            }
            let mut state = lock(&self.shared)?;
            state.stats.commit_calls = state.stats.commit_calls.saturating_add(1);
            // 自有文件是 direct mode；不用事务，也不把 flush 成功冒充磁盘满注入。
            state.file.sync_all().map_err(|error| state.io_error(error))
        })
    }

    fn Revert(&self) -> windows::core::Result<()> {
        // direct mode 没有可回滚的事务；既有字节及所有产物保留。
        guarded(|| Ok(()))
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows::core::Result<()> {
        guarded(|| Err(Error::from_hresult(E_NOTIMPL)))
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows::core::Result<()> {
        guarded(|| Err(Error::from_hresult(E_NOTIMPL)))
    }

    fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        guarded(|| {
            if pstatstg.is_null() {
                return Err(Error::from_hresult(E_POINTER));
            }
            unsafe { pstatstg.write(STATSTG::default()) };
            if ![STATFLAG_DEFAULT, STATFLAG_NONAME, STATFLAG_NOOPEN].contains(grfstatflag) {
                return Err(Error::from_hresult(STG_E_INVALIDFLAG));
            }
            let mut state = lock(&self.shared)?;
            state.refresh_len()?;
            // 此 COM 流没有逻辑名称，pwcsName 保持空；无需向调用者转移名称内存。
            let stat = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: state.stats.actual_len,
                grfMode: STGM_READWRITE | STGM_SHARE_DENY_WRITE,
                ..Default::default()
            };
            unsafe { pstatstg.write(stat) };
            Ok(())
        })
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        guarded(|| {
            let cursor = *lock(&self.cursor)?;
            let mut state = lock(&self.shared)?;
            state.stats.clone_calls = state.stats.clone_calls.saturating_add(1);
            drop(state);
            Ok(FaultStream {
                shared: Arc::clone(&self.shared),
                cursor: Mutex::new(cursor),
            }
            .into())
        })
    }
}

#[test]
#[ignore = "Creates a private file and calls COM/MF; run only in the native verification batch"]
fn recording_fault_stream_native_contract() -> crate::Result<()> {
    use serde_json::json;
    use windows::Win32::System::Com::STGC_DEFAULT;

    let root = Path::new("target/recording-storage-fault");
    std::fs::create_dir_all(root)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let directory = (0..128)
        .find_map(|serial| {
            let path = root.join(format!(
                "stream-contract-{}-{stamp}-{serial}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => Some(Ok(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .ok_or("无法创建独占故障流测试目录")??;
    let result = (|| -> crate::Result<serde_json::Value> {
        // 按正常调用合同，Runtime 最后释放；这里只测流，不创建编码器或播放器。
        let _runtime = crate::media_foundation::MfRuntime::new()?;
        let path = directory.join("stream.bin");
        let (stream, control) = create_stream(&path)?;
        let _adapter = unsafe { MFCreateMFByteStreamOnStream(&stream) }?;
        let initial = [1_u8, 2, 3, 4, 5, 6, 7, 8];
        let mut written = 0;
        let initial_hr = unsafe {
            stream.Write(
                initial.as_ptr().cast(),
                initial.len() as u32,
                Some(&mut written),
            )
        };
        initial_hr.ok()?;
        let initial_written = written;
        unsafe { stream.Seek(2, STREAM_SEEK_SET, None) }?;
        let overwrite = [9_u8, 10];
        let overwrite_hr =
            unsafe { stream.Write(overwrite.as_ptr().cast(), 2, Some(&mut written)) };
        overwrite_hr.ok()?;
        let overwrite_written = written;
        let before_arm = control.snapshot()?;
        let arm_below_len_rejected = control.arm(7).is_err();
        let arm_above_cap_rejected = control.arm(SAFETY_CAP + 1).is_err();
        control.arm(8)?;
        unsafe { stream.Seek(7, STREAM_SEEK_SET, None) }?;
        written = 99;
        let rejected_write =
            unsafe { stream.Write(overwrite.as_ptr().cast(), 2, Some(&mut written)) };
        let rejected_written = written;
        let rejected_set_size = unsafe { stream.SetSize(9) }
            .err()
            .map(|error| error.code().0 as u32);

        unsafe { stream.Seek(2, STREAM_SEEK_SET, None) }?;
        let cloned = unsafe { stream.Clone() }?;
        let mut clone_bytes = [0_u8; 3];
        let mut read = 0;
        let clone_read =
            unsafe { cloned.Read(clone_bytes.as_mut_ptr().cast(), 3, Some(&mut read)) };
        let clone_read_count = read;
        let mut original_cursor = 0;
        let mut clone_cursor = 0;
        unsafe {
            stream.Seek(0, STREAM_SEEK_CUR, Some(&mut original_cursor))?;
            cloned.Seek(0, STREAM_SEEK_CUR, Some(&mut clone_cursor))?;
            stream.Seek(7, STREAM_SEEK_SET, None)?;
        }
        let mut eof_bytes = [0_u8; 4];
        let short_read = unsafe { stream.Read(eof_bytes.as_mut_ptr().cast(), 4, Some(&mut read)) };
        let short_read_count = read;
        let mut large_cursor = 0;
        unsafe { stream.Seek(-1, STREAM_SEEK_SET, Some(&mut large_cursor)) }?;
        let zero_write = unsafe { stream.Write(initial.as_ptr().cast(), 0, None) };
        let huge_read = unsafe { stream.Read(eof_bytes.as_mut_ptr().cast(), 1, Some(&mut read)) };
        let huge_read_count = read;
        let overflow_seek = unsafe { stream.Seek(1, STREAM_SEEK_CUR, None) }
            .err()
            .map(|error| error.code().0 as u32);
        unsafe { stream.Seek(0, STREAM_SEEK_SET, None) }?;
        let atomic_limit = control.arm_after_current_len(1)?;
        let zero_slack_rejected = control.arm_after_current_len(0).is_err();
        let mut stat = STATSTG::default();
        unsafe {
            stream.Stat(&mut stat, STATFLAG_NONAME)?;
            stream.Commit(STGC_DEFAULT)?;
        }
        let before_collision = std::fs::read(&path)?;
        let collision_rejected = create(&path).is_err();
        let after_collision = std::fs::read(&path)?;
        let final_snapshot = control.snapshot()?;
        let contract_ok = initial_hr == S_OK
            && initial_written == 8
            && overwrite_hr == S_OK
            && overwrite_written == 2
            && before_arm.actual_len == 8
            && before_arm.high_water == 8
            && !before_arm.armed
            && before_arm.injected_count == 0
            && arm_below_len_rejected
            && arm_above_cap_rejected
            && rejected_write == STG_E_MEDIUMFULL
            && rejected_written == 0
            && rejected_set_size == Some(STG_E_MEDIUMFULL.0 as u32)
            && clone_read == S_OK
            && clone_read_count == 3
            && clone_bytes == [9, 10, 5]
            && original_cursor == 2
            && clone_cursor == 5
            && short_read == S_FALSE
            && short_read_count == 1
            && eof_bytes == [8, 0, 0, 0]
            && large_cursor == u64::MAX
            && zero_write == S_OK
            && huge_read == S_FALSE
            && huge_read_count == 0
            && overflow_seek == Some(STG_E_INVALIDFUNCTION.0 as u32)
            && atomic_limit == 9
            && zero_slack_rejected
            && stat.cbSize == 8
            && stat.r#type == STGTY_STREAM.0 as u32
            && collision_rejected
            && before_collision == [1, 2, 9, 10, 5, 6, 7, 8]
            && after_collision == before_collision
            && final_snapshot.actual_len == 8
            && final_snapshot.high_water == 8
            && final_snapshot.limit == 9
            && final_snapshot.arm_calls == 2
            && final_snapshot.cap_hits == 2
            && final_snapshot.injected_count == 2
            && final_snapshot.injected_write_count == 1
            && final_snapshot.injected_set_size_count == 1
            && final_snapshot.first_injected_hresult == Some(STG_E_MEDIUMFULL.0 as u32);
        Ok(json!({
            "contract_ok": contract_ok,
            "before_arm": before_arm,
            "final_snapshot": final_snapshot,
            "rejected_write_hresult": rejected_write.0 as u32,
            "rejected_set_size_hresult": rejected_set_size,
            "rejected_written": rejected_written,
            "clone_bytes": clone_bytes,
            "original_cursor": original_cursor,
            "clone_cursor": clone_cursor,
            "short_read_hresult": short_read.0 as u32,
            "short_read_count": short_read_count,
            "large_cursor": large_cursor,
            "huge_read_hresult": huge_read.0 as u32,
            "huge_read_count": huge_read_count,
            "overflow_seek_hresult": overflow_seek,
            "atomic_limit": atomic_limit,
            "zero_slack_rejected": zero_slack_rejected,
            "collision_rejected": collision_rejected,
            "retained_bytes": after_collision,
        }))
    })();
    let report = match &result {
        Ok(evidence) => json!({ "evidence": evidence }),
        Err(error) => json!({ "error": error.to_string().chars().take(1024).collect::<String>() }),
    };
    let report = json!({
        "scope": "Private seekable file-stream contract with injected STG_E_MEDIUMFULL; not physical disk exhaustion, MP4 compatibility, or worker recovery evidence. Limits cover file byte length/extent, not filesystem allocation, metadata, or process memory. All artifacts are retained.",
        "result": report,
    });
    let bytes = serde_json::to_vec_pretty(&report)?;
    if bytes.len() > 64 * 1024 {
        return Err("故障流测试报告超出 64 KiB 上限".into());
    }
    let report_path = directory.join("report.json");
    let mut report_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&report_path)?;
    report_file.write_all(&bytes)?;
    report_file.flush()?;
    report_file.sync_all()?;
    eprintln!("Fault stream contract report: {}", report_path.display());
    assert_eq!(
        report["result"]["evidence"]["contract_ok"].as_bool(),
        Some(true),
        "{}",
        report
    );
    Ok(())
}
