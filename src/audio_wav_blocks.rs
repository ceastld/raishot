//! 保存混音使用的有界 WAV 读取器，仅接受录屏自有的 48 kHz 立体声 PCM16。
use crate::Result;
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_READ,
};

const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const BLOCK_ALIGN: u64 = 4;
const MAX_CHUNKS: usize = 128;
pub(crate) const MAX_READ_FRAMES: usize = 48_000;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    bytes: u64,
    written: u64,
}

fn local_stamp(metadata: &Metadata) -> Result<Stamp> {
    let unavailable = FILE_ATTRIBUTE_OFFLINE.0
        | FILE_ATTRIBUTE_RECALL_ON_OPEN.0
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0
        | FILE_ATTRIBUTE_REPARSE_POINT.0;
    if !metadata.is_file() || metadata.file_attributes() & unavailable != 0 {
        return Err(tr!(
            "录屏 WAV 必须是已在本机的普通文件，不能是云端占位或链接",
            "Recording WAV must be a local regular file, not a cloud placeholder or link"
        )
        .into());
    }
    Ok(Stamp {
        bytes: metadata.len(),
        written: metadata.last_write_time(),
    })
}

fn invalid_chunks() -> Box<dyn std::error::Error> {
    tr!(
        "录屏 WAV 块长度无效或文件不完整",
        "Recording WAV has invalid chunk lengths or is incomplete"
    )
    .into()
}

fn read_exact(reader: &mut impl Read, bytes: &mut [u8]) -> Result<()> {
    reader.read_exact(bytes).map_err(|error| {
        trf!(
            "读取录屏 WAV 失败（文件可能被截短）：{error}",
            "Could not read recording WAV (it may have been truncated): {error}"
        )
        .into()
    })
}

fn locate_data(file: &mut File, file_bytes: u64) -> Result<(u64, u64)> {
    let mut riff = [0_u8; 12];
    read_exact(file, &mut riff)?;
    if riff[0..4] != *b"RIFF" || riff[8..12] != *b"WAVE" {
        return Err(tr!("录屏音轨不是 RIFF WAV", "Recording audio is not RIFF WAV").into());
    }
    let end = u64::from(u32::from_le_bytes(riff[4..8].try_into()?))
        .checked_add(8)
        .ok_or_else(invalid_chunks)?;
    if end != file_bytes || end < 12 {
        return Err(invalid_chunks());
    }

    let mut position = 12_u64;
    let mut format_seen = false;
    let mut data = None;
    for _ in 0..MAX_CHUNKS {
        if position == end {
            if format_seen {
                return data.ok_or_else(|| {
                    tr!("录屏 WAV 缺少 data 块", "Recording WAV has no data chunk").into()
                });
            }
            return Err(tr!("录屏 WAV 缺少 fmt 块", "Recording WAV has no fmt chunk").into());
        }
        let payload = position.checked_add(8).ok_or_else(invalid_chunks)?;
        if payload > end {
            return Err(invalid_chunks());
        }
        file.seek(SeekFrom::Start(position))?;
        let mut header = [0_u8; 8];
        read_exact(file, &mut header)?;
        let bytes = u64::from(u32::from_le_bytes(header[4..8].try_into()?));
        let payload_end = payload.checked_add(bytes).ok_or_else(invalid_chunks)?;
        let next = payload_end
            .checked_add(bytes & 1)
            .ok_or_else(invalid_chunks)?;
        if next > end {
            return Err(invalid_chunks());
        }
        match &header[0..4] {
            b"fmt " => {
                if format_seen {
                    return Err(tr!(
                        "录屏 WAV 有重复的 fmt 块",
                        "Recording WAV has duplicate fmt chunks"
                    )
                    .into());
                }
                if bytes != 16 {
                    return Err(tr!(
                        "录屏 WAV 不是固定 PCM16 格式",
                        "Recording WAV is not the fixed PCM16 format"
                    )
                    .into());
                }
                let mut format = [0_u8; 16];
                read_exact(file, &mut format)?;
                let tag = u16::from_le_bytes(format[0..2].try_into()?);
                let channels = u16::from_le_bytes(format[2..4].try_into()?);
                let rate = u32::from_le_bytes(format[4..8].try_into()?);
                let bytes_per_second = u32::from_le_bytes(format[8..12].try_into()?);
                let align = u16::from_le_bytes(format[12..14].try_into()?);
                let bits = u16::from_le_bytes(format[14..16].try_into()?);
                if tag != 1
                    || channels != CHANNELS as u16
                    || rate != RATE
                    || bytes_per_second != RATE * BLOCK_ALIGN as u32
                    || u64::from(align) != BLOCK_ALIGN
                    || bits != 16
                {
                    return Err(tr!(
                        "录屏 WAV 必须是 48 kHz 立体声 16 位 PCM",
                        "Recording WAV must be 48 kHz stereo 16-bit PCM"
                    )
                    .into());
                }
                format_seen = true;
            }
            b"data" => {
                if data.is_some() {
                    return Err(tr!(
                        "录屏 WAV 有重复的 data 块",
                        "Recording WAV has duplicate data chunks"
                    )
                    .into());
                }
                if bytes % BLOCK_ALIGN != 0 {
                    return Err(tr!(
                        "录屏 WAV 数据不是完整立体声帧",
                        "Recording WAV data has an incomplete stereo frame"
                    )
                    .into());
                }
                data = Some((payload, bytes));
            }
            // 未知块只跳过位置；不按块声明长度分配或读入其内容。
            _ => {}
        }
        position = next;
    }
    // 最后一个允许的块恰好结束仍有效，不多读一个块头。
    if position == end && format_seen {
        return data
            .ok_or_else(|| tr!("录屏 WAV 缺少 data 块", "Recording WAV has no data chunk").into());
    }
    Err(tr!(
        "录屏 WAV 块数量超过读取上限",
        "Recording WAV exceeds the chunk count limit"
    )
    .into())
}

fn read_pcm_block(reader: &mut impl Read, frames: usize) -> Result<Vec<f32>> {
    if frames > MAX_READ_FRAMES {
        return Err(tr!(
            "单次 WAV 读取不能超过 48000 帧",
            "A WAV read cannot exceed 48000 frames"
        )
        .into());
    }
    // 上限与文件声明长度无关：原始字节最多 192000，输出最多 96000 个样本。
    let mut bytes = vec![0_u8; frames * BLOCK_ALIGN as usize];
    read_exact(reader, &mut bytes)?;
    Ok(bytes
        .chunks_exact(2)
        .map(|sample| f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32768.)
        .collect())
}

pub(crate) struct WavBlockReader {
    file: File,
    stamp: Stamp,
    data_start: u64,
    frames: u64,
    consumed: u64,
}

impl WavBlockReader {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let before = local_stamp(&fs::symlink_metadata(path)?)?;
        // 不共享写入或删除；不跟随末级重解析对象，读内容前复核句柄属性。
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_OPEN_NO_RECALL.0)
            .open(path)
            .map_err(|error| {
                trf!(
                    "无法锁定只读录屏 WAV（文件可能仍在写入）：{error}",
                    "Could not lock recording WAV for reading (it may still be written): {error}"
                )
            })?;
        let stamp = local_stamp(&file.metadata()?)?;
        if before != stamp {
            return Err(tr!(
                "打开期间录屏 WAV 已变化",
                "Recording WAV changed while opening"
            )
            .into());
        }
        let (data_start, bytes) = locate_data(&mut file, stamp.bytes)?;
        let reader = Self {
            file,
            stamp,
            data_start,
            frames: bytes / BLOCK_ALIGN,
            consumed: 0,
        };
        reader.check_unchanged()?;
        Ok(reader)
    }

    pub(crate) fn frames(&self) -> u64 {
        self.frames
    }

    fn check_unchanged(&self) -> Result<()> {
        if local_stamp(&self.file.metadata()?)? != self.stamp {
            return Err(tr!(
                "读取期间录屏 WAV 已变化，已停止混音",
                "Recording WAV changed while reading; mixing stopped"
            )
            .into());
        }
        Ok(())
    }

    /// 最多读取一秒；尾块不补零，读完后继续返回空。零或超上限请求报错。
    pub(crate) fn read(&mut self, max_frames: usize) -> Result<Vec<f32>> {
        if max_frames == 0 || max_frames > MAX_READ_FRAMES {
            return Err(tr!(
                "单次 WAV 读取帧数必须为 1–48000",
                "A WAV read must request 1–48000 frames"
            )
            .into());
        }
        self.check_unchanged()?;
        let count = (self.frames - self.consumed).min(max_frames as u64) as usize;
        if count == 0 {
            return Ok(Vec::new());
        }
        let offset = self
            .consumed
            .checked_mul(BLOCK_ALIGN)
            .and_then(|bytes| self.data_start.checked_add(bytes))
            .ok_or_else(invalid_chunks)?;
        // 短读失败不推进已消费帧数，下一次读取也不会跳过部分数据。
        self.file.seek(SeekFrom::Start(offset))?;
        let samples = read_pcm_block(&mut self.file, count)?;
        self.check_unchanged()?;
        self.consumed += count as u64;
        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav_track::WavWriter;
    use std::{
        ffi::c_void,
        io::{Cursor, Write},
        os::windows::io::AsRawHandle,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture {
        directory: PathBuf,
        path: PathBuf,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "rustshot-wav-blocks-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory)?;
            Ok(Self {
                path: directory.join("track.wav"),
                directory,
            })
        }

        fn write(&self, bytes: &[u8]) -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&self.path)?;
            file.write_all(bytes)?;
            Ok(())
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // 只清理本测试新建的一个文件及空目录，不递归删除。
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_dir(&self.directory);
        }
    }

    fn wave(data: &[u8], junk: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&RATE.to_le_bytes());
        bytes.extend_from_slice(&(RATE * 4).to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        if junk {
            bytes.extend_from_slice(b"JUNK\x03\0\0\0abc\0");
        }
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() % 2 != 0 {
            bytes.push(0);
        }
        let riff_bytes = (bytes.len() - 8) as u32;
        bytes[4..8].copy_from_slice(&riff_bytes.to_le_bytes());
        bytes
    }

    #[test]
    fn writer_blocks_return_exact_tail_and_repeat_empty_eof() -> Result<()> {
        let fixture = Fixture::new()?;
        let samples: Vec<f32> = (0..MAX_READ_FRAMES + 3)
            .flat_map(|frame| {
                let value = match frame % 3 {
                    0 => -1.,
                    1 => 0.25,
                    _ => 0.75,
                };
                [value, -value]
            })
            .collect();
        let mut writer = WavWriter::create(&fixture.path)?;
        for chunk in samples.chunks(8000) {
            writer.write(chunk)?;
        }
        writer.finish()?;
        let mut reader = WavBlockReader::open(&fixture.path)?;
        assert_eq!(reader.frames(), MAX_READ_FRAMES as u64 + 3);
        let first = reader.read(MAX_READ_FRAMES)?;
        let tail = reader.read(MAX_READ_FRAMES)?;
        assert_eq!(first.len(), MAX_READ_FRAMES * CHANNELS);
        assert_eq!(tail.len(), 3 * CHANNELS);
        for (actual, source) in first.iter().chain(&tail).zip(samples) {
            let expected = f32::from((source.clamp(-1., 1.) * 32767.) as i16) / 32768.;
            assert_eq!(*actual, expected);
        }
        assert!(reader.read(1)?.is_empty());
        assert!(reader.read(MAX_READ_FRAMES)?.is_empty());
        assert!(reader.read(0).is_err());
        assert!(reader.read(MAX_READ_FRAMES + 1).is_err());
        Ok(())
    }

    #[test]
    fn odd_unknown_chunk_is_skipped_without_shifting_stereo_frames() -> Result<()> {
        let fixture = Fixture::new()?;
        let values = [i16::MIN, i16::MAX, 123, -456, 0, 1];
        let data: Vec<u8> = values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        fixture.write(&wave(&data, true))?;
        let mut reader = WavBlockReader::open(&fixture.path)?;
        assert_eq!(reader.frames(), 3);
        let mut samples = reader.read(2)?;
        samples.extend(reader.read(2)?);
        let expected: Vec<f32> = values
            .iter()
            .map(|value| f32::from(*value) / 32768.)
            .collect();
        assert_eq!(samples, expected);
        assert!(reader.read(2)?.is_empty());
        Ok(())
    }

    #[test]
    fn rejects_wrong_format_declared_lengths_and_incomplete_frames() -> Result<()> {
        let valid = wave(&[0; 4], false);
        let mut invalid = Vec::new();
        for (offset, replacement) in [
            (20, 3_u16.to_le_bytes().to_vec()),
            (22, 1_u16.to_le_bytes().to_vec()),
            (24, 44_100_u32.to_le_bytes().to_vec()),
            (28, 1_u32.to_le_bytes().to_vec()),
            (32, 2_u16.to_le_bytes().to_vec()),
            (34, 8_u16.to_le_bytes().to_vec()),
            (40, 8_u32.to_le_bytes().to_vec()),
        ] {
            let mut bytes = valid.clone();
            bytes[offset..offset + replacement.len()].copy_from_slice(&replacement);
            invalid.push(bytes);
        }
        invalid.push(wave(&[0; 2], false));
        invalid.push(valid[..valid.len() - 1].to_vec());
        let mut trailing = valid.clone();
        trailing.push(0);
        invalid.push(trailing);
        for (case, bytes) in invalid.iter().enumerate() {
            let fixture = Fixture::new()?;
            fixture.write(bytes)?;
            assert!(WavBlockReader::open(&fixture.path).is_err(), "case {case}");
        }
        Ok(())
    }

    #[test]
    fn short_pcm_read_is_an_error_without_zero_padding() {
        let mut truncated = Cursor::new([0_u8; 7]);
        assert!(read_pcm_block(&mut truncated, 2).is_err());
        assert!(read_pcm_block(&mut Cursor::new([0_u8; 0]), MAX_READ_FRAMES + 1).is_err());
    }

    #[test]
    fn read_handle_denies_writing_and_removal_until_drop() -> Result<()> {
        let fixture = Fixture::new()?;
        fixture.write(&wave(&[0; 4], false))?;
        let reader = WavBlockReader::open(&fixture.path)?;
        assert!(OpenOptions::new().write(true).open(&fixture.path).is_err());
        assert!(fs::remove_file(&fixture.path).is_err());
        drop(reader);
        assert!(OpenOptions::new().write(true).open(&fixture.path).is_ok());
        Ok(())
    }

    #[test]
    fn empty_data_is_valid_and_chunk_scan_has_a_fixed_limit() -> Result<()> {
        let empty = Fixture::new()?;
        empty.write(&wave(&[], false))?;
        let mut reader = WavBlockReader::open(&empty.path)?;
        assert_eq!(reader.frames(), 0);
        assert!(reader.read(1)?.is_empty());
        assert!(reader.read(MAX_READ_FRAMES)?.is_empty());

        let mut bytes = wave(&[0; 4], false);
        let padding = b"JUNK\0\0\0\0".repeat(MAX_CHUNKS);
        bytes.splice(12..12, padding);
        let riff_bytes = (bytes.len() - 8) as u32;
        bytes[4..8].copy_from_slice(&riff_bytes.to_le_bytes());
        let excessive = Fixture::new()?;
        excessive.write(&bytes)?;
        assert!(WavBlockReader::open(&excessive.path).is_err());
        Ok(())
    }

    // 只为自造测试文件标记稀疏属性，不扩大生产依赖的 Windows feature。
    #[link(name = "kernel32")]
    unsafe extern "system" {
        #[link_name = "DeviceIoControl"]
        fn device_io_control(
            handle: *mut c_void,
            code: u32,
            input: *const c_void,
            input_len: u32,
            output: *mut c_void,
            output_len: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
    }

    #[test]
    fn sparse_hundreds_of_megabytes_read_only_one_bounded_block() -> Result<()> {
        const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
        const DATA_BYTES: u32 = 256 * 1024 * 1024;
        let fixture = Fixture::new()?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&fixture.path)?;
        let mut returned = 0_u32;
        // 空输入表示启用 sparse；后续扩展只创建零值空洞，不写数百 MB 内容。
        let marked = unsafe {
            device_io_control(
                file.as_raw_handle(),
                FSCTL_SET_SPARSE,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if marked == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut header = wave(&[], false);
        header[4..8].copy_from_slice(&(36 + DATA_BYTES).to_le_bytes());
        header[40..44].copy_from_slice(&DATA_BYTES.to_le_bytes());
        file.write_all(&header)?;
        file.set_len(44 + u64::from(DATA_BYTES))?;
        file.flush()?;
        drop(file);

        let mut reader = WavBlockReader::open(&fixture.path)?;
        assert_eq!(reader.frames(), u64::from(DATA_BYTES) / BLOCK_ALIGN);
        let first = reader.read(MAX_READ_FRAMES)?;
        assert_eq!(first.len(), MAX_READ_FRAMES * CHANNELS);
        assert!(first.iter().all(|sample| *sample == 0.));
        assert_eq!(reader.consumed, MAX_READ_FRAMES as u64);
        assert!(reader.consumed < reader.frames());
        Ok(())
    }
}
