//! 普通文件按同一打开句柄的长度冻结读取；来源变化时不扩容，也不保留第二份压缩图。
use crate::Result;
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const READ_CHUNK_BYTES: usize = 64 * 1024;

fn checkpoint(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr!("图片读取已取消", "Image reading canceled").into())
    } else {
        Ok(())
    }
}

fn changed() -> Box<dyn std::error::Error> {
    tr!(
        "图片来源在读取时发生变化",
        "Image source changed while reading"
    )
    .into()
}

fn checked_capacity(capacity: u64) -> Result<usize> {
    checked_capacity_with_resident(capacity, 0)
}

fn checked_capacity_with_resident(capacity: u64, external_resident_bytes: u64) -> Result<usize> {
    if external_resident_bytes
        .checked_add(capacity)
        .and_then(|bytes| bytes.checked_add(READ_CHUNK_BYTES as u64))
        .is_none_or(|bytes| bytes > super::MAX_DECODE_BYTES)
    {
        return Err(tr!(
            "图片来源与读取工作区超过256MiB预算",
            "Image source and read workspace exceed the 256 MiB budget"
        )
        .into());
    }
    usize::try_from(capacity)
        .map_err(|_| tr!("图片来源长度溢出", "Image source length overflow").into())
}

fn read_chunk(reader: &mut impl Read, output: &mut [u8], cancel: &AtomicBool) -> Result<usize> {
    loop {
        checkpoint(cancel)?;
        let result = reader.read(output);
        checkpoint(cancel)?;
        match result {
            Ok(count) if count <= output.len() => return Ok(count),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    tr!(
                        "图片来源返回了无效读取长度",
                        "Image source returned an invalid read length"
                    ),
                )
                .into())
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

pub(super) fn read_known_length(
    reader: &mut impl Read,
    length: u64,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    read_known_length_with_resident(reader, length, cancel, 0, super::MAX_DECODE_BYTES)
}

fn read_known_length_with_resident(
    reader: &mut impl Read,
    length: u64,
    cancel: &AtomicBool,
    external_resident_bytes: u64,
    max_input_bytes: u64,
) -> Result<Vec<u8>> {
    checkpoint(cancel)?;
    if length > max_input_bytes.min(super::MAX_DECODE_BYTES) {
        return Err(tr!(
            "图片来源超过本次读取上限",
            "Image source exceeds this read limit"
        )
        .into());
    }
    let length = checked_capacity_with_resident(length, external_resident_bytes)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| tr!("图片压缩数据分配失败", "Image input allocation failed"))?;
    checked_capacity_with_resident(bytes.capacity() as u64, external_resident_bytes)?;
    let mut part = [0; READ_CHUNK_BYTES];
    while bytes.len() < length {
        let wanted = (length - bytes.len()).min(part.len());
        let count = read_chunk(reader, &mut part[..wanted], cancel)?;
        if count == 0 {
            return Err(changed());
        }
        // 已为固定长度一次分配，分片只写入既有容量；源增长不触发新旧Vec共存。
        bytes.extend_from_slice(&part[..count]);
    }
    if read_chunk(reader, &mut part[..1], cancel)? != 0 {
        return Err(changed());
    }
    checkpoint(cancel)?;
    Ok(bytes)
}

/// 路径资格在打开前检查；长度与像素编码字节来自同一文件句柄。
/// 格式解析、原生解码与显示副本的工作区由后续阶段另行计费。
pub(super) fn read_path_input(path: &Path, cancel: &AtomicBool) -> Result<Vec<u8>> {
    read_path_input_with_resident(path, cancel, 0, super::MAX_DECODE_BYTES)
}

/// 外部驻留在资格检查和读入期间继续计费；来源上限在压缩Vec分配前检查。
pub(super) fn read_path_input_with_resident(
    path: &Path,
    cancel: &AtomicBool,
    external_resident_bytes: u64,
    max_input_bytes: u64,
) -> Result<Vec<u8>> {
    let result = (|| {
        checkpoint(cancel)?;
        checked_capacity_with_resident(0, external_resident_bytes)?;
        let local = crate::image_collection::ensure_local_file(path);
        checkpoint(cancel)?;
        local?;
        let opened = File::open(path);
        checkpoint(cancel)?;
        let mut file = opened?;
        let metadata = file.metadata();
        checkpoint(cancel)?;
        let metadata = metadata?;
        if !metadata.is_file() {
            return Err(tr!("图片来源不是普通文件", "Image source is not a regular file").into());
        }
        read_known_length_with_resident(
            &mut file,
            metadata.len(),
            cancel,
            external_resident_bytes,
            max_input_bytes,
        )
    })();
    checkpoint(cancel)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn image_decode_input_reserves_the_fixed_workspace_before_reading() {
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("source budget must reject before reading");
            }
        }
        let cancel = AtomicBool::new(false);
        let largest = super::super::MAX_DECODE_BYTES - READ_CHUNK_BYTES as u64;
        assert_eq!(checked_capacity(largest).unwrap() as u64, largest);
        assert!(checked_capacity(largest + 1).is_err());
        assert!(checked_capacity(u64::MAX).is_err());
        assert!(read_known_length(&mut NoRead, largest + 1, &cancel).is_err());
        assert!(read_known_length(&mut NoRead, u64::MAX, &cancel).is_err());
    }

    #[test]
    fn image_decode_input_external_resident_and_input_limit_reject_before_reading() {
        struct NoRead;
        impl Read for NoRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                panic!("resident and input limit must reject before reading");
            }
        }
        let cancel = AtomicBool::new(false);
        let resident = super::super::MAX_DECODE_BYTES - READ_CHUNK_BYTES as u64 - 8;
        assert_eq!(checked_capacity_with_resident(8, resident).unwrap(), 8);
        assert!(checked_capacity_with_resident(9, resident).is_err());
        assert!(checked_capacity_with_resident(8, resident + 1).is_err());
        assert!(checked_capacity_with_resident(1, u64::MAX).is_err());
        assert!(read_known_length_with_resident(&mut NoRead, 8, &cancel, resident + 1, 8).is_err());
        let error = read_known_length_with_resident(&mut NoRead, 9, &cancel, 0, 8)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("读取上限") || error.contains("read limit"));
        assert_eq!(
            read_known_length_with_resident(&mut Cursor::new(b"abc"), 3, &cancel, 1024, 3).unwrap(),
            b"abc"
        );
    }

    #[test]
    fn image_decode_input_accepts_partial_reads_without_growing_the_frozen_buffer() {
        struct Partial {
            input: Cursor<&'static [u8]>,
            calls: usize,
        }
        impl Read for Partial {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                let count = output.len().min(2);
                self.input.read(&mut output[..count])
            }
        }
        let mut reader = Partial {
            input: Cursor::new(b"abcdef"),
            calls: 0,
        };
        let bytes = read_known_length(&mut reader, 6, &AtomicBool::new(false)).unwrap();
        assert_eq!(bytes, b"abcdef");
        assert_eq!(reader.calls, 4);
        assert!(bytes.capacity() >= 6);
        assert!(checked_capacity(bytes.capacity() as u64).is_ok());
        assert!(
            read_known_length(&mut Cursor::new(b""), 0, &AtomicBool::new(false))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn image_decode_input_shortened_and_grown_sources_are_explicit_changes() {
        for (input, length) in [(&b"ab"[..], 3), (&b"abc"[..], 2)] {
            let error = read_known_length(&mut Cursor::new(input), length, &AtomicBool::new(false))
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("发生变化") || error.contains("changed while reading"));
        }
    }

    #[test]
    fn image_decode_input_retries_interrupted_reads_and_preserves_other_io_errors() {
        struct InterruptOnce {
            interrupted: bool,
            input: Cursor<&'static [u8]>,
        }
        impl Read for InterruptOnce {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.input.read(output)
            }
        }
        let mut reader = InterruptOnce {
            interrupted: false,
            input: Cursor::new(b"ab"),
        };
        assert_eq!(
            read_known_length(&mut reader, 2, &AtomicBool::new(false)).unwrap(),
            b"ab"
        );
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "source denied",
                ))
            }
        }
        let error = read_known_length(&mut Broken, 1, &AtomicBool::new(false))
            .err()
            .unwrap();
        assert!(error.to_string().contains("source denied"));
    }

    #[test]
    fn image_decode_input_cancellation_wins_over_read_errors_and_the_final_eof_check() {
        struct CancelRead<'a> {
            cancel: &'a AtomicBool,
            first_byte: bool,
            calls: usize,
        }
        impl Read for CancelRead<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.first_byte {
                    self.first_byte = false;
                    output[0] = b'a';
                    return Ok(1);
                }
                self.cancel.store(true, Ordering::Relaxed);
                Err(io::ErrorKind::Interrupted.into())
            }
        }
        for first_byte in [false, true] {
            let cancel = AtomicBool::new(false);
            let mut reader = CancelRead {
                cancel: &cancel,
                first_byte,
                calls: 0,
            };
            let error = read_known_length(&mut reader, 1, &cancel)
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("取消") || error.contains("canceled"));
            assert_eq!(reader.calls, if first_byte { 2 } else { 1 });
            assert!(read_known_length(&mut reader, 1, &cancel).is_err());
            assert_eq!(reader.calls, if first_byte { 2 } else { 1 });
        }
        let error = read_path_input(
            Path::new("missing-image-source.png"),
            &AtomicBool::new(true),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("取消") || error.contains("canceled"));
    }
}
