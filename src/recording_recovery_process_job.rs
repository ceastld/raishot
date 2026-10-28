#![cfg(all(test, windows))]

use std::{
    ffi::c_void,
    io,
    mem::{align_of, offset_of, size_of, zeroed},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    process::Child,
    ptr,
};

const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

// SDK JOBOBJECT_BASIC_LIMIT_INFORMATION：LARGE_INTEGER 为 i64，SIZE_T 和
// ULONG_PTR 为 usize。repr(C) 按目标 ABI 排列，不手工填充结构体。
#[repr(C)]
struct BasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
struct ExtendedLimitInformation {
    basic_limit_information: BasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

const _: () = {
    assert!(size_of::<IoCounters>() == 48);
    assert!(align_of::<IoCounters>() == 8);
    assert!(offset_of!(BasicLimitInformation, limit_flags) == 16);
    assert!(offset_of!(ExtendedLimitInformation, basic_limit_information) == 0);
};

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<BasicLimitInformation>() == 64);
    assert!(size_of::<ExtendedLimitInformation>() == 144);
    assert!(offset_of!(BasicLimitInformation, minimum_working_set_size) == 24);
    assert!(offset_of!(BasicLimitInformation, affinity) == 48);
    assert!(offset_of!(ExtendedLimitInformation, io_info) == 64);
    assert!(offset_of!(ExtendedLimitInformation, process_memory_limit) == 112);
};

#[cfg(target_pointer_width = "32")]
const _: () = {
    assert!(size_of::<BasicLimitInformation>() == 48);
    assert!(size_of::<ExtendedLimitInformation>() == 112);
    assert!(offset_of!(BasicLimitInformation, minimum_working_set_size) == 20);
    assert!(offset_of!(BasicLimitInformation, affinity) == 32);
    assert!(offset_of!(ExtendedLimitInformation, io_info) == 48);
    assert!(offset_of!(ExtendedLimitInformation, process_memory_limit) == 96);
};

#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "CreateJobObjectW"]
    fn create_job_object(attributes: *const c_void, name: *const u16) -> *mut c_void;
    #[link_name = "SetInformationJobObject"]
    fn set_information_job_object(
        job: *mut c_void,
        information_class: i32,
        information: *mut c_void,
        information_length: u32,
    ) -> i32;
    #[link_name = "AssignProcessToJobObject"]
    fn assign_process_to_job_object(job: *mut c_void, process: *mut c_void) -> i32;
}

/// 恢复原生试验专用的、不可继承的匿名 Job 句柄。
///
/// 调用方必须先成功 assign，再通过管道许可子进程启动 MF；关联失败时不能
/// 发许可。应保留此 guard 直到观察到子进程退出。关闭最后一个 Job 句柄会
/// 请求终止其进程，但这不是进程退出或 I/O 完成的墙钟时限保证。
#[derive(Debug)]
pub(super) struct JobGuard {
    handle: OwnedHandle,
}

impl JobGuard {
    pub(super) fn new() -> io::Result<Self> {
        // NULL SECURITY_ATTRIBUTES 明确禁止继承句柄；NULL 名称不复用其它 Job。
        let raw = unsafe { create_job_object(ptr::null(), ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // CreateJobObjectW 已转移一个有效、独占持有的内核句柄。
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        // 所有字段均为整数；全零有效，只启用 kill-on-close，不加其它限制。
        let mut limits: ExtendedLimitInformation = unsafe { zeroed() };
        limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            set_information_job_object(
                handle.as_raw_handle(),
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&mut limits as *mut ExtendedLimitInformation).cast(),
                size_of::<ExtendedLimitInformation>() as u32,
            )
        };
        if ok == 0 {
            // 必须在 OwnedHandle 析构关闭句柄前取得本次原生错误。
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    pub(super) fn assign(&self, child: &Child) -> io::Result<()> {
        // 借用 std::Child 所持的进程句柄，避免 PID 查找及句柄所有权转移。
        let ok = unsafe {
            assign_process_to_job_object(self.handle.as_raw_handle(), child.as_raw_handle())
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
