//! 录屏历史首帧缩略图；只在后台读取受保护的成片，不创建播放或音频会话。
use super::{downscale, Thumb, THUMB_HEIGHT, THUMB_WIDTH};
use crate::{
    file_snapshot::{self, FileSnapshot},
    media_foundation::{MfRuntime, VideoReader},
    recording_library_model::Entry,
    Result,
};
use std::{
    fs::{File, OpenOptions},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};

const MAX_PIXELS: u64 = 32_000_000;
const MAX_SIDE: u32 = 65_535;
const UNAVAILABLE_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;

pub(super) fn load(entry: &Entry, cancelled: &dyn Fn() -> bool) -> Result<Option<Thumb>> {
    check_cancel(cancelled)?;
    let result = (|| {
        crate::image_collection::ensure_local_file(&entry.path)?;
        crate::recording_library_model::check_thumbnail_source(&entry.path)?;
        check_cancel(cancelled)?;
        load_file(entry, cancelled)
    })();
    check_cancel(cancelled)?;
    result
}

/// 固定库资格由调用入口检查；测试仅以本次自行创建的文件检验相同读取路径。
fn load_file(entry: &Entry, cancelled: &dyn Fn() -> bool) -> Result<Option<Thumb>> {
    check_cancel(cancelled)?;
    let result = (|| {
        let path = std::path::absolute(&entry.path)?;
        let guards = ReadGuards::open(&path, entry)?;
        check_cancel(cancelled)?;
        // 与截图历史共用并发预算；等待只在后台发生，换代或关窗可以退出。
        let _permit = loop {
            check_cancel(cancelled)?;
            if let Some(permit) = crate::history::decode_gate().try_enter() {
                break permit;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let runtime = MfRuntime::new();
        check_cancel(cancelled)?;
        let _runtime = runtime?;
        // 保持调用方的文件路径表达；不把规范化得到的命名空间前缀强加给媒体后端。
        let reader = VideoReader::open(&path);
        check_cancel(cancelled)?;
        let mut reader = reader?;
        frame_bytes(reader.width, reader.height)?;
        check_cancel(cancelled)?;
        // 系统内部的单次同步调用无法由此闭包中断；前后均拒绝过期结果。
        let frame = reader.next();
        check_cancel(cancelled)?;
        let frame = frame?;
        let thumb = match frame {
            Some(frame) => {
                let expected = frame_bytes(reader.width, reader.height)?;
                if frame.bgra.len() != expected {
                    return Err(tr!(
                        "录屏首帧像素长度无效",
                        "Invalid recording first-frame pixel length"
                    )
                    .into());
                }
                Some(downscale(
                    reader.width,
                    reader.height,
                    &frame.bgra,
                    THUMB_WIDTH,
                    THUMB_HEIGHT,
                ))
            }
            None => None,
        };
        check_cancel(cancelled)?;
        guards.validate(entry)?;
        check_cancel(cancelled)?;
        // 局部变量逆序释放：先 Reader，再 MF/COM，最后文件及目录保护。
        Ok(thumb)
    })();
    check_cancel(cancelled)?;
    result
}

fn frame_bytes(width: u32, height: u32) -> Result<usize> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || pixels > MAX_PIXELS {
        return Err(tr!(
            "录屏首帧尺寸超出缩略图预算",
            "Recording first-frame dimensions exceed the thumbnail budget"
        )
        .into());
    }
    pixels
        .checked_mul(4)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or_else(|| {
            tr!(
                "录屏首帧字节数无效",
                "Invalid recording first-frame byte count"
            )
            .into()
        })
}

fn check_cancel(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(tr!("缩略图解码已取消", "Thumbnail decoding cancelled").into())
    } else {
        Ok(())
    }
}

struct ReadGuards {
    path: PathBuf,
    file: File,
    before: FileSnapshot,
    // 只禁止目录重命名/删除；共享写入允许其它录制照常在该目录创建成片。
    directory: File,
}
impl ReadGuards {
    fn open(path: &Path, entry: &Entry) -> Result<Self> {
        crate::image_collection::ensure_local_file(path)?;
        let parent = path.parent().ok_or_else(|| {
            tr!(
                "录屏文件没有父目录",
                "The recording file has no parent folder"
            )
        })?;
        let directory = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(parent)?;
        check_directory(&directory)?;
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)?;
        check_entry(entry, &file.metadata()?)?;
        let before = file_snapshot::snapshot(&file)?;
        let guards = Self {
            path: path.to_owned(),
            file,
            before,
            directory,
        };
        guards.validate(entry)?;
        Ok(guards)
    }

    fn validate(&self, entry: &Entry) -> Result<()> {
        check_directory(&self.directory)?;
        crate::image_collection::ensure_local_file(&self.path)?;
        check_entry(entry, &self.file.metadata()?)?;
        if file_snapshot::snapshot(&self.file)? != self.before
            || file_snapshot::inspect(&self.path)?.as_ref() != Some(&self.before)
        {
            return Err(tr!(
                "录屏文件已改变，请刷新后再试",
                "The recording file changed; refresh and try again"
            )
            .into());
        }
        Ok(())
    }
}

fn check_directory(directory: &File) -> Result<()> {
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & UNAVAILABLE_ATTRIBUTES != 0 {
        return Err(tr!(
            "录屏目录不是可用的本地普通目录",
            "The recording folder is not an available local folder"
        )
        .into());
    }
    Ok(())
}

fn check_entry(entry: &Entry, metadata: &std::fs::Metadata) -> Result<()> {
    if !metadata.is_file()
        || metadata.file_attributes() & UNAVAILABLE_ATTRIBUTES != 0
        || metadata.len() == 0
        || metadata.len() != entry.bytes
        || metadata.modified().ok() != entry.modified
    {
        return Err(tr!(
            "录屏文件已改变，请刷新后再试",
            "The recording file changed; refresh and try again"
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows::Win32::Storage::FileSystem::FILE_SHARE_DELETE;

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rustshot-library-thumbnail-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn entry(path: &Path) -> Entry {
        let metadata = std::fs::metadata(path).unwrap();
        Entry {
            path: path.to_owned(),
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
            info: Default::default(),
        }
    }

    #[test]
    fn library_recording_thumbnail_frame_budget_precedes_pixels() {
        assert_eq!(frame_bytes(8_000, 4_000).unwrap(), 128_000_000);
        assert_eq!(frame_bytes(65_535, 1).unwrap(), 262_140);
        for (width, height) in [
            (0, 1),
            (1, 0),
            (65_536, 1),
            (1, 65_536),
            (8_001, 4_000),
            (u32::MAX, u32::MAX),
        ] {
            assert!(frame_bytes(width, height).is_err());
        }
    }

    #[test]
    fn library_recording_thumbnail_guards_block_replacement_without_blocking_new_recordings() {
        let directory = TestDirectory::new();
        let source = directory.0.join("first.mp4");
        std::fs::write(&source, b"owned source").unwrap();
        let entry = entry(&source);
        let guards = ReadGuards::open(&source, &entry).unwrap();
        let old = file_snapshot::snapshot(&guards.file).unwrap();
        assert!(OpenOptions::new()
            .write(true)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .open(&source)
            .is_err());
        assert!(std::fs::rename(&source, directory.0.join("moved.mp4")).is_err());
        let renamed_directory = directory.0.with_extension("moved");
        assert!(std::fs::rename(&directory.0, &renamed_directory).is_err());
        let second = directory.0.join("second.mp4");
        std::fs::write(&second, b"new independent recording").unwrap();
        guards.validate(&entry).unwrap();
        assert_eq!(file_snapshot::snapshot(&guards.file).unwrap(), old);
        drop(guards);
        std::fs::write(&source, b"replacement is now allowed").unwrap();
        assert!(ReadGuards::open(&source, &entry).is_err());
        assert_eq!(std::fs::read(second).unwrap(), b"new independent recording");
    }

    #[test]
    fn library_recording_thumbnail_cancel_precedes_file_or_fixed_root_access() {
        let entry = Entry {
            path: PathBuf::from("not-an-accessible-file\0.mp4"),
            name: "cancelled.mp4".into(),
            bytes: 1,
            modified: None,
            info: Default::default(),
        };
        let expected = check_cancel(&|| true).unwrap_err().to_string();
        for result in [load(&entry, &|| true), load_file(&entry, &|| true)] {
            match result {
                Err(error) => assert_eq!(error.to_string(), expected),
                Ok(_) => panic!("cancelled thumbnail accepted an inaccessible source"),
            }
        }
    }

    #[test]
    #[ignore = "显式运行自有H.264首帧读取，不打开窗口或播放声音"]
    fn library_recording_thumbnail_native_first_frame_and_source_identity() -> Result<()> {
        use crate::media_foundation::Mp4Writer;

        let directory = TestDirectory::new();
        let source = directory.0.join("缩略图 # sample.mp4");
        let (width, height) = (640_u32, 360_u32);
        {
            let _runtime = MfRuntime::new()?;
            let mut writer = Mp4Writer::new(&source, width, height, 10, 1_500_000, false)?;
            let mut pixels = vec![0_u8; frame_bytes(width, height)?];
            for index in 0..3 {
                for pixel in pixels.chunks_exact_mut(4) {
                    pixel.copy_from_slice(if index == 0 {
                        &[25, 20, 230, 255]
                    } else {
                        &[230, 20, 25, 255]
                    });
                }
                writer.write_video(
                    &pixels,
                    width,
                    Duration::from_millis(index * 100),
                    Duration::from_millis(100),
                )?;
            }
            writer.finish()?;
        }
        let entry = entry(&source);
        let before = file_snapshot::inspect(&source)?.ok_or("missing owned recording")?;
        let original = std::fs::read(&source)?;
        let thumb = load_file(&entry, &|| false)?.ok_or("missing first-frame thumbnail")?;
        assert_eq!((thumb.width, thumb.height), (480, 270));
        assert_eq!(thumb.bgra.len(), 480 * 270 * 4);
        assert!(thumb
            .bgra
            .chunks_exact(4)
            .all(|pixel| pixel[2] > 180 && pixel[0] < 80 && pixel[3] == 255));
        {
            let _runtime = MfRuntime::new()?;
            let mut reader = VideoReader::open(&source)?;
            let first = reader.next()?.ok_or("missing reference first frame")?;
            assert_eq!(first.time, Duration::ZERO);
            let expected = downscale(width, height, &first.bgra, THUMB_WIDTH, THUMB_HEIGHT);
            assert_eq!(thumb.bgra, expected.bgra);
        }
        assert_eq!(file_snapshot::inspect(&source)?, Some(before));
        assert_eq!(std::fs::read(&source)?, original);
        assert_eq!(std::fs::read_dir(&directory.0)?.count(), 1);
        Ok(())
    }
}
