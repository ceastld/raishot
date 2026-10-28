//! 普通文件的原生身份快照；只描述对象，不自行授予发布或删除资格。
//! 属性检查与随后按路径操作不是外部文件系统 CAS。
use crate::Result;
use std::{
    fs::{File, OpenOptions},
    io,
    mem::size_of,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileSnapshot {
    pub(crate) volume: u64,
    pub(crate) id: [u8; 16],
    pub(crate) created: u64,
    pub(crate) written: u64,
    pub(crate) bytes: u64,
    pub(crate) attributes: u32,
}

/// 缺失为 None；拒绝目录与末级重解析对象，不跟随末级链接取得目标资格。
pub(crate) fn inspect(path: &Path) -> Result<Option<FileSnapshot>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(tr!(
                "文件不是普通文件，未取得身份",
                "Cannot identify a non-regular file"
            )
            .into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let file = OpenOptions::new()
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    Ok(Some(snapshot(&file)?))
}

pub(crate) fn snapshot(file: &File) -> Result<FileSnapshot> {
    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(handle, &mut info) }?;
    if info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT).0 != 0 {
        return Err(tr!(
            "文件不是普通文件，未取得身份",
            "Cannot identify a non-regular file"
        )
        .into());
    }
    // 同一已打开句柄读取完整128-bit身份；ReFS不保证旧64-bit FileIndex唯一。
    // 读取失败直接拒绝资格，不能回退到旧索引或仅靠路径／bytes判定相同对象。
    let mut identity = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            (&mut identity as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    }?;
    Ok(FileSnapshot {
        volume: identity.VolumeSerialNumber,
        id: identity.FileId.Identifier,
        created: u64::from(info.ftCreationTime.dwHighDateTime) << 32
            | u64::from(info.ftCreationTime.dwLowDateTime),
        written: u64::from(info.ftLastWriteTime.dwHighDateTime) << 32
            | u64::from(info.ftLastWriteTime.dwLowDateTime),
        bytes: u64::from(info.nFileSizeHigh) << 32 | u64::from(info.nFileSizeLow),
        attributes: info.dwFileAttributes,
    })
}
