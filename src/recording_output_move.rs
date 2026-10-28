//! 将已核验的公开源对象移回私有暂存；源身份与 rename 绑定同一个句柄。
//! 目标由调用方提供已知暂存路径；这里不实现祖先链保护、跨卷复制或路径移动回退。
use crate::{
    file_snapshot::{snapshot, FileSnapshot},
    Result,
};
use std::{
    fs::OpenOptions,
    mem::{offset_of, size_of, MaybeUninit},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
    ptr::{addr_of_mut, copy_nonoverlapping},
};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        FileRenameInfo, SetFileInformationByHandle, DELETE, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_READ,
    },
};

pub(super) fn restore_identified(
    source: &Path,
    destination: &Path,
    expected: &FileSnapshot,
) -> Result<()> {
    if source.as_os_str().encode_wide().any(|unit| unit == 0)
        || destination.as_os_str().encode_wide().any(|unit| unit == 0)
    {
        return Err(tr!(
            "录屏回移路径含有 NUL，未移动文件",
            "The recording restore path contains a NUL; no file was moved"
        )
        .into());
    }
    let destination = std::path::absolute(destination)?;
    if !destination.is_absolute() || destination.file_name().is_none() {
        return Err(tr!(
            "录屏回移目标不是完整文件路径",
            "The recording restore destination is not a complete file path"
        )
        .into());
    }
    let name_units = destination.as_os_str().encode_wide().count();
    let name_bytes = name_units.checked_mul(size_of::<u16>()).ok_or(tr!(
        "录屏回移目标名称长度溢出",
        "The recording restore name length overflowed"
    ))?;
    let name_length = u32::try_from(name_bytes)?;
    let terminated_units = name_units.checked_add(1).ok_or(tr!(
        "录屏回移目标名称长度溢出",
        "The recording restore name length overflowed"
    ))?;
    // FileNameLength 不包含末尾 NUL；完整结构之外再分配全部名称与 NUL，留足 ABI 尾部余量。
    let buffer_bytes = size_of::<FILE_RENAME_INFO>()
        .checked_add(name_bytes)
        .and_then(|bytes| bytes.checked_add(size_of::<u16>()))
        .ok_or(tr!(
            "录屏回移缓冲区长度溢出",
            "The recording restore buffer length overflowed"
        ))?;
    let buffer_size = u32::try_from(buffer_bytes)?;
    let mut name = Vec::new();
    name.try_reserve_exact(terminated_units)?;
    name.extend(destination.as_os_str().encode_wide());
    name.push(0);

    // MaybeUninit 保留结构的原生对齐；字符串可能覆盖后续槽位，不能将这些槽位当有效 bool 结构。
    let slots = buffer_bytes.div_ceil(size_of::<FILE_RENAME_INFO>());
    let mut storage = Vec::<MaybeUninit<FILE_RENAME_INFO>>::new();
    storage.try_reserve_exact(slots)?;
    storage.resize_with(slots, MaybeUninit::uninit);
    let base = storage.as_mut_ptr().cast::<u8>();
    let information = base.cast::<FILE_RENAME_INFO>();
    // SAFETY: 整块分配按 FILE_RENAME_INFO 对齐且全部清零。字段均在首个结构内；尾字符串
    // 从分配 base + SDK offset_of 派生，不经单元素 FileName 字段借用向后扩写。上方已检查
    // 名称字节数、NUL 和总缓冲长度，分配至少 buffer_bytes；两个分配不重叠且均存活到 FFI 返回。
    unsafe {
        // 在最终分配上逐字节清零，连同 SDK 结构 padding 一起初始化供 FFI 读取的完整范围。
        base.write_bytes(0, buffer_bytes);
        addr_of_mut!((*information).Anonymous.ReplaceIfExists).write(false);
        addr_of_mut!((*information).RootDirectory).write(HANDLE::default());
        addr_of_mut!((*information).FileNameLength).write(name_length);
        copy_nonoverlapping(
            name.as_ptr(),
            base.add(offset_of!(FILE_RENAME_INFO, FileName))
                .cast::<u16>(),
            terminated_units,
        );
    }

    // 仅请求 DELETE 与属性读取；拒共享写/删除，已有不兼容 writer/rename 句柄会令打开失败。
    // OPEN_REPARSE_POINT 不跟随末级链接；同句柄 snapshot 还会拒目录、重解析及任何元数据差异。
    let source_file = OpenOptions::new()
        .read(true)
        .access_mode((DELETE | FILE_READ_ATTRIBUTES).0)
        .share_mode(FILE_SHARE_READ.0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(source)?;
    if &snapshot(&source_file)? != expected {
        return Err(tr!(
            "录屏公开文件身份或元数据已变化，未回移现有文件",
            "The public recording identity or metadata changed; the existing file was not restored"
        )
        .into());
    }
    // SAFETY: source_file 始终持有已核验对象的 DELETE 句柄。上方构造的是 SDK FileRenameInfo
    // 对应结构，RootDirectory=null 与完整绝对路径配套；ReplaceIfExists=false，目标存在即失败。
    // 仅对该源句柄 rename；任何失败原样返回，不按路径重开、删除或退回 MoveFileEx。
    unsafe {
        SetFileInformationByHandle(
            HANDLE(source_file.as_raw_handle()),
            FileRenameInfo,
            base.cast(),
            buffer_size,
        )?;
    }
    Ok(())
}
