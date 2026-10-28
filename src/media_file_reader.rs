//! 以文件路径打开媒体字节流，避免把 Windows 文件命名空间交给 URL scheme 解析。
use std::{os::windows::ffi::OsStrExt, path::Path};
use windows::{
    core::{Error, Interface, Result, HSTRING, PCWSTR},
    Win32::{
        Foundation::{E_INVALIDARG, E_UNEXPECTED},
        Media::MediaFoundation::*,
    },
};

fn file_path_text(path: &Path) -> Result<HSTRING> {
    // PCWSTR 会在首个 NUL 截断；拒绝它才能保证实际打开的文件与调用方路径一致。
    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(Error::new(
            E_INVALIDARG,
            tr!(
                "媒体文件路径包含空字符",
                "The media file path contains a null character"
            ),
        ));
    }
    Ok(HSTRING::from(path.as_os_str()))
}

/// 调用线程持有 COM/MF 生命周期。文件路径不转换或重新规范化，身份检查仍由调用方负责。
pub(crate) fn open_file_source_reader(
    path: &Path,
    attributes: Option<&IMFAttributes>,
) -> Result<IMFSourceReader> {
    let path_text = file_path_text(path)?;
    unsafe {
        let mut owned_attributes = None;
        MFCreateAttributes(&mut owned_attributes, 1)?;
        let owned_attributes = owned_attributes.ok_or_else(|| {
            Error::new(
                E_UNEXPECTED,
                tr!(
                    "无法创建媒体读取属性",
                    "Cannot create media reader attributes"
                ),
            )
        })?;
        if let Some(attributes) = attributes {
            attributes.CopyAllItems(&owned_attributes)?;
        }
        // 这是本函数独占创建的文件媒体源，必须随 Reader 释放而 Shutdown。
        owned_attributes.SetUINT32(&MF_SOURCE_READER_DISCONNECT_MEDIASOURCE_ON_SHUTDOWN, 0)?;
        let file = MFCreateFile(
            MF_ACCESSMODE_READ,
            MF_OPENMODE_FAIL_IF_NOT_EXIST,
            MF_FILEFLAGS_NONE,
            PCWSTR(path_text.as_ptr()),
        )?;
        let result = (|| -> Result<IMFSourceReader> {
            let resolver = MFCreateSourceResolver()?;
            let mut object_type = MF_OBJECT_INVALID;
            let mut object = None;
            // 文件已经打开；原路径只提供扩展名及来源上下文，不再经 scheme handler 打开。
            resolver.CreateObjectFromByteStream(
                &file,
                PCWSTR(path_text.as_ptr()),
                MF_RESOLUTION_MEDIASOURCE.0 as u32,
                None,
                &mut object_type,
                &mut object,
            )?;
            if object_type != MF_OBJECT_MEDIASOURCE {
                return Err(Error::new(
                    E_UNEXPECTED,
                    tr!(
                        "文件解析器未返回媒体源",
                        "The file resolver returned no media source"
                    ),
                ));
            }
            let object = object.ok_or_else(|| {
                Error::new(
                    E_UNEXPECTED,
                    tr!(
                        "文件解析器返回了空对象",
                        "The file resolver returned an empty object"
                    ),
                )
            })?;
            let source = object.cast::<IMFMediaSource>()?;
            match MFCreateSourceReaderFromMediaSource(&source, &owned_attributes) {
                Ok(reader) => Ok(reader),
                Err(error) => {
                    // Reader 没有接管已创建的源，失败分支也要结束源的活动对象。
                    let _ = source.Shutdown();
                    Err(error)
                }
            }
        })();
        if result.is_err() {
            let _ = file.Close();
        }
        // 成功时 Reader 持有媒体源，媒体源持有字节流；这里只释放本地引用，不提前 Close。
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};

    #[test]
    fn rejects_null_that_would_open_a_different_file() {
        let path = Path::new("C:\\media\\original.mp4\0different.mp4");
        assert_eq!(file_path_text(path).unwrap_err().code(), E_INVALIDARG);
    }

    #[test]
    fn preserves_drive_unc_extended_paths_and_utf16_exactly() {
        for text in [
            "C:\\媒体\\a #%.mp4",
            "\\\\server\\share\\a.mp4",
            "\\\\?\\C:\\媒体\\a.mp4. ",
            "\\\\?\\UNC\\server\\share\\a.mp4",
        ] {
            let path = Path::new(text);
            let expected: Vec<_> = path.as_os_str().encode_wide().collect();
            assert_eq!(&*file_path_text(path).unwrap(), expected.as_slice());
        }
        let mut units: Vec<_> = "\\\\?\\C:\\media\\".encode_utf16().collect();
        units.push(0xd800);
        units.extend(".mp4".encode_utf16());
        let name = OsString::from_wide(&units);
        assert_eq!(
            &*file_path_text(Path::new(&name)).unwrap(),
            units.as_slice()
        );
    }
}
