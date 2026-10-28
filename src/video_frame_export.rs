//! 当前视频帧的只读快照。保留显示帧的 Arc，导出期间播放可继续而不替换输出内容。
use crate::{capture::Image, Result};
use gpui::RenderImage;
use std::{
    fs::{File, OpenOptions},
    io::{self, BufWriter, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::HWND,
        Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH},
    },
};

const MAX_FRAME_PIXELS: u64 = 32_000_000;
const MAX_FILE_NAME_UTF16_UNITS: usize = 255;
const FRAME_CLIPBOARD_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// 点击复制时仍由查看窗口保留的CPU像素；同一帧的多个Arc只记一次。
/// 与剪贴板准备的可见缓冲预算相加，不代表播放器、GPU或系统的总内存峰值。
pub(crate) fn retained_render_bytes<'a>(
    images: impl IntoIterator<Item = &'a Arc<RenderImage>>,
) -> u64 {
    let mut seen = std::collections::HashSet::new();
    let mut bytes = 0_u64;
    for image in images {
        if seen.insert(Arc::as_ptr(image)) {
            for frame in 0..image.frame_count() {
                bytes =
                    bytes.saturating_add(image.as_bytes(frame).map_or(0, |data| data.len() as u64));
            }
        }
    }
    bytes
}

#[derive(Clone)]
pub(crate) struct FrameSnapshot {
    image: Arc<RenderImage>,
    source: PathBuf,
    pub(crate) position: Duration,
    width: u32,
    height: u32,
}

impl FrameSnapshot {
    /// `position` 必须来自这一场景提交帧，不能在保存对话框返回后重新取值。
    pub(crate) fn new(
        image: Arc<RenderImage>,
        source: PathBuf,
        position: Duration,
    ) -> Result<Self> {
        let bytes = image.as_bytes(0).ok_or(tr!(
            "当前视频帧像素不完整",
            "The current video frame is incomplete"
        ))?;
        let size = image.size(0);
        let (width, height) = (u32::from(size.width), u32::from(size.height));
        let pixels = u64::from(width) * u64::from(height);
        if pixels == 0 || pixels > MAX_FRAME_PIXELS {
            return Err(tr!(
                "当前视频帧过大或尺寸无效，无法输出",
                "The current video frame is too large or has an invalid size"
            )
            .into());
        }
        let expected = usize::try_from(pixels * 4)?;
        if bytes.len() != expected {
            return Err(tr!(
                "当前视频帧像素不完整",
                "The current video frame is incomplete"
            )
            .into());
        }
        Ok(Self {
            image,
            source,
            position,
            width,
            height,
        })
    }

    pub(crate) fn suggested_name(&self) -> String {
        // 毫秒区分同一秒内不同帧；后缀先占预算，文件名不含时间冒号。
        let suffix = format!("_{}ms.png", self.position.as_millis());
        let mut budget = MAX_FILE_NAME_UTF16_UNITS.saturating_sub(suffix.encode_utf16().count());
        let stem = self
            .source
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy();
        // 按完整字符截断，不把补充平面字符拆成孤立UTF-16代理项。
        let stem: String = stem
            .chars()
            .take_while(|character| {
                let units = character.len_utf16();
                if units > budget {
                    return false;
                }
                budget -= units;
                true
            })
            .collect();
        let stem = if stem.is_empty() { "Video" } else { &stem };
        format!("{stem}{suffix}")
    }

    pub(crate) fn directory(&self) -> &Path {
        self.source
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
    }

    pub(crate) fn to_image(&self) -> Image {
        Image {
            width: self.width,
            height: self.height,
            bgra: self
                .image
                .as_bytes(0)
                .expect("validated video frame")
                .to_vec(),
            readback_ms: 0.,
        }
    }

    pub(crate) fn copy(&self, owner: HWND) -> Result<()> {
        self.prepare_clipboard(&AtomicBool::new(false), 0)?
            .publish(owner)
    }

    /// 后台按行拷贝；调用方回到 UI 后仍须检查窗口、来源与输出意图是否有效。
    pub(crate) fn prepare_pixels(&self, cancel: &AtomicBool) -> Result<Image> {
        check_cancel(cancel)?;
        let pixels = self.image.as_bytes(0).expect("validated video frame");
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(pixels.len())?;
        for row in pixels.chunks_exact(self.width as usize * 4) {
            check_cancel(cancel)?;
            bgra.extend_from_slice(row);
        }
        check_cancel(cancel)?;
        Ok(Image {
            width: self.width,
            height: self.height,
            bgra,
            readback_ms: 0.,
        })
    }

    /// 只准备原生载荷，不打开剪贴板；来源和剪贴板序列仍由窗口在发布前确认。
    pub(crate) fn prepare_clipboard(
        &self,
        cancel: &AtomicBool,
        retained_bytes: u64,
    ) -> Result<crate::export::PreparedImageClipboard> {
        let image = self.prepare_pixels(cancel)?;
        // 快照即使已从窗口场景换下，仍保留其像素和来源名；另计本次转换Vec的实际容量。
        let resident_bytes = retained_bytes
            .max(retained_render_bytes([&self.image]))
            .saturating_add(self.source.as_os_str().encode_wide().count() as u64 * 2)
            .saturating_add(image.bgra.capacity() as u64);
        crate::export::prepare_image_clipboard_with_budget_cancellable(
            &image,
            cancel,
            resident_bytes,
            FRAME_CLIPBOARD_MAX_BYTES,
        )
    }

    /// 仅编码至独占暂存文件，不发布目标；UI 确认意图仍有效后再 commit。
    pub(crate) fn prepare_new_png(
        &self,
        chosen: &Path,
        cancel: &AtomicBool,
    ) -> Result<PreparedFrameSave> {
        check_cancel(cancel)?;
        let target = png_target(chosen)?;
        protect_source(&self.source, &target)?;
        match std::fs::symlink_metadata(&target) {
            Ok(_) => return Err(existing_target().into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let (prepared, file) = PreparedFrameSave::create(target, self.source.clone())?;
        // 文件句柄归编码函数所有，失败返回时先关闭，再由 prepared 清理。
        encode_staged(
            file,
            self.width,
            self.height,
            self.image.as_bytes(0).expect("validated video frame"),
            cancel,
        )?;
        check_cancel(cancel)?;
        Ok(prepared)
    }

    /// 同步便捷入口；有窗口/来源生命周期的输出应分开 prepare 与 commit。
    pub(crate) fn save_new_png(&self, chosen: &Path, cancel: &AtomicBool) -> Result<PathBuf> {
        self.prepare_new_png(chosen, cancel)?.commit(cancel)
    }
}

/// 只拥有 create_new 成功的暂存路径；未提交或提交失败时自动移除它。
/// 不可 Clone，不能把同一暂存同时交给两个发布意图。
pub(crate) struct PreparedFrameSave {
    staged: Option<PathBuf>,
    target: PathBuf,
    source: PathBuf,
}

impl PreparedFrameSave {
    fn create(target: PathBuf, source: PathBuf) -> Result<(Self, File)> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..64 {
            let staged = target.with_file_name(format!(
                ".rustshot-video-frame-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)
            {
                Ok(file) => {
                    return Ok((
                        Self {
                            staged: Some(staged),
                            target,
                            source,
                        },
                        file,
                    ))
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(tr!(
            "无法创建视频帧暂存文件，请重试",
            "Cannot create the video frame staging file; try again"
        )
        .into())
    }

    /// UI 收回准备结果且确认意图仍有效后调用；同目录原子发布，从不替换目标。
    pub(crate) fn commit(mut self, cancel: &AtomicBool) -> Result<PathBuf> {
        check_cancel(cancel)?;
        protect_source(&self.source, &self.target)?;
        let staged = self
            .staged
            .as_ref()
            .expect("owned video frame staging file");
        let source: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
        let target: Vec<u16> = self
            .target
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        check_cancel(cancel)?;
        // 不带 REPLACE_EXISTING/COPY_ALLOWED：检查后出现同名文件也不会被覆盖。
        unsafe {
            MoveFileExW(
                PCWSTR(source.as_ptr()),
                PCWSTR(target.as_ptr()),
                MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| {
            let error = io::Error::from_raw_os_error((error.code().0 as u32 & 0xffff) as i32);
            if error.kind() == io::ErrorKind::AlreadyExists {
                existing_target()
            } else {
                error
            }
        })?;
        self.staged = None;
        Ok(self.target.clone())
    }
}

impl Drop for PreparedFrameSave {
    fn drop(&mut self) {
        if let Some(path) = self.staged.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn png_target(chosen: &Path) -> Result<PathBuf> {
    let chosen = match chosen.extension() {
        None => chosen.with_extension("png"),
        Some(extension) if extension.is_empty() => chosen.with_extension("png"),
        Some(extension) if extension.eq_ignore_ascii_case("png") => chosen.to_path_buf(),
        _ => {
            return Err(tr!(
                "当前视频帧请保存为 PNG 文件",
                "Save the current video frame as a PNG file"
            )
            .into())
        }
    };
    let name = chosen.file_name().ok_or(tr!(
        "视频帧保存文件名无效",
        "The video frame file name is invalid"
    ))?;
    // 目标必须是普通文件；提前拒绝命名流和内嵌终止符。
    if name.to_string_lossy().contains(['\0', ':']) {
        return Err(tr!(
            "视频帧保存文件名无效",
            "The video frame file name is invalid"
        )
        .into());
    }
    let absolute = std::path::absolute(&chosen)?;
    // 固定已存在的父目录并保留 verbatim/UNC，支持长路径且目标不必已存在。
    let parent = std::fs::canonicalize(absolute.parent().ok_or(tr!(
        "视频帧保存目标没有父目录",
        "The video frame target has no parent folder"
    ))?)?;
    Ok(parent.join(name))
}

fn protect_source(source: &Path, target: &Path) -> Result<()> {
    if crate::export::is_same_path(source, target) {
        return Err(tr!(
            "不能覆盖原视频文件",
            "The original video cannot be overwritten"
        )
        .into());
    }
    Ok(())
}

fn existing_target() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        tr!(
            "目标文件已存在，请选择新文件名",
            "The file already exists; choose a new name"
        ),
    )
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Acquire) {
        // 不用 Interrupted，避免 Write::write_all 在取消后自动重试。
        return Err(io::Error::other(tr!(
            "已取消视频帧输出",
            "Video frame export canceled"
        )));
    }
    Ok(())
}

struct CancelWriter<'a, W> {
    writer: W,
    cancel: &'a AtomicBool,
}

impl<W: Write> Write for CancelWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check_cancel(self.cancel)?;
        self.writer.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        check_cancel(self.cancel)?;
        self.writer.flush()
    }
}

fn encode_staged(
    mut file: File,
    width: u32,
    height: u32,
    bgra: &[u8],
    cancel: &AtomicBool,
) -> Result<()> {
    encode_frame_png(BufWriter::new(&mut file), width, height, bgra, cancel)?;
    check_cancel(cancel)?;
    file.sync_all()?;
    check_cancel(cancel)?;
    Ok(())
}

fn encode_frame_png(
    writer: impl Write,
    width: u32,
    height: u32,
    bgra: &[u8],
    cancel: &AtomicBool,
) -> Result<()> {
    let mut output = CancelWriter { writer, cancel };
    // 保留压缩；Flate2路径会传播取消/写出错误，避免快速编码器终结时panic。
    crate::export::write_png_pixels(width, height, bgra, &mut output, png::Compression::Balanced)?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "rustshot-video-frame-test-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            // 只清理本次独占创建的目录内文件，不递归遍历或删除子目录。
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    fn frame(bytes: Vec<u8>) -> Arc<RenderImage> {
        Arc::new(RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::from_raw(1, 1, bytes).unwrap(),
        )]))
    }

    fn snapshot(source: PathBuf) -> FrameSnapshot {
        FrameSnapshot::new(
            frame(vec![4, 20, 240, 255]),
            source,
            Duration::from_millis(1234),
        )
        .unwrap()
    }

    #[test]
    fn snapshot_retains_selected_frame_and_time() {
        let first = frame(vec![4, 20, 240, 255]);
        let snapshot = FrameSnapshot::new(
            first.clone(),
            PathBuf::from("sample.mp4"),
            Duration::from_millis(1234),
        )
        .unwrap();
        let next = frame(vec![0, 0, 0, 255]);
        assert!(!Arc::ptr_eq(&first, &next));
        assert!(Arc::ptr_eq(&snapshot.image, &first));
        assert_eq!(snapshot.to_image().bgra, vec![4, 20, 240, 255]);
        assert_eq!(snapshot.suggested_name(), "sample_1234ms.png");
        assert!(!snapshot.suggested_name().contains(':'));
    }

    #[test]
    fn retained_frame_budget_counts_distinct_buffers_without_arc_aliases() {
        let first = frame(vec![4, 20, 240, 255]);
        let alias = first.clone();
        let separate = frame(vec![4, 20, 240, 255]);
        let multiple = Arc::new(RenderImage::new(vec![
            image::Frame::new(image::RgbaImage::from_raw(1, 1, vec![1, 2, 3, 255]).unwrap()),
            image::Frame::new(image::RgbaImage::from_raw(1, 1, vec![4, 5, 6, 255]).unwrap()),
        ]));
        assert_eq!(
            retained_render_bytes([&first, &alias, &separate, &multiple, &multiple]),
            16
        );
    }

    #[test]
    fn frame_clipboard_preparation_respects_retained_budget_and_cancellation() {
        let snapshot = snapshot(PathBuf::from("sample.mp4"));
        let cancel = AtomicBool::new(false);
        // 只准备并释放HGLOBAL，不打开或发布系统剪贴板。
        drop(snapshot.prepare_clipboard(&cancel, 0).unwrap());
        assert!(snapshot
            .prepare_clipboard(&cancel, FRAME_CLIPBOARD_MAX_BYTES)
            .is_err());
        assert!(snapshot.prepare_clipboard(&cancel, u64::MAX).is_err());
        cancel.store(true, Ordering::Release);
        assert!(snapshot.prepare_clipboard(&cancel, 0).is_err());
        assert_eq!(snapshot.to_image().bgra, [4, 20, 240, 255]);
    }

    #[test]
    fn suggested_names_bound_long_ascii_chinese_and_emoji_stems_without_io() {
        for stem in ["a".repeat(251), "中".repeat(251), "🎬".repeat(125)] {
            let source = PathBuf::from(format!("{stem}.mp4"));
            assert!(source.as_os_str().encode_wide().count() <= 255);
            for position in [Duration::ZERO, Duration::from_millis(1234), Duration::MAX] {
                let snapshot =
                    FrameSnapshot::new(frame(vec![4, 20, 240, 255]), source.clone(), position)
                        .unwrap();
                let name = snapshot.suggested_name();
                let units = name.encode_utf16().count();
                assert!(units <= 255);
                assert!(units >= 254);
                let suffix = format!("_{}ms.png", position.as_millis());
                let shortened = name.strip_suffix(&suffix).unwrap();
                assert!(!shortened.is_empty());
                assert!(stem.starts_with(shortened));
                assert!(!shortened.contains('\u{fffd}'));
            }
        }
        assert_eq!(
            snapshot(PathBuf::new()).suggested_name(),
            "Video_1234ms.png"
        );
    }

    #[test]
    fn preparing_without_commit_never_publishes_and_drop_removes_staging() {
        let directory = TestDirectory::new();
        let snapshot = snapshot(directory.0.join("sample.mp4"));
        let target = directory.0.join("frame.png");
        let prepared = snapshot
            .prepare_new_png(&target, &AtomicBool::new(false))
            .unwrap();
        let staged = prepared.staged.clone().unwrap();
        assert!(staged.is_file());
        assert!(!target.exists());
        drop(prepared);
        assert!(!staged.exists());
        assert!(!target.exists());
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
    }

    #[test]
    fn cancel_before_prepare_or_commit_leaves_no_output_or_staging() {
        let directory = TestDirectory::new();
        let snapshot = snapshot(directory.0.join("sample.mp4"));
        let destination = directory.0.join("canceled-frame.png");
        let cancel = AtomicBool::new(true);
        assert!(snapshot.save_new_png(&destination, &cancel).is_err());
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
        cancel.store(false, Ordering::Release);
        let prepared = snapshot.prepare_new_png(&destination, &cancel).unwrap();
        let staged = prepared.staged.clone().unwrap();
        cancel.store(true, Ordering::Release);
        assert!(prepared.commit(&cancel).is_err());
        assert!(!destination.exists());
        assert!(!staged.exists());
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 0);
        assert!(snapshot.prepare_pixels(&cancel).is_err());
        cancel.store(false, Ordering::Release);
        assert_eq!(
            snapshot.prepare_pixels(&cancel).unwrap().bgra,
            vec![4, 20, 240, 255]
        );
    }

    #[test]
    fn new_target_is_png_and_keeps_the_original_video() {
        let directory = TestDirectory::new();
        let source = directory.0.join("sample.mp4");
        std::fs::write(&source, b"original video bytes").unwrap();
        let snapshot = snapshot(source.clone());
        let cancel = AtomicBool::new(false);
        let target = snapshot
            .prepare_new_png(&directory.0.join("frame"), &cancel)
            .unwrap()
            .commit(&cancel)
            .unwrap();
        assert!(target.is_absolute());
        assert_eq!(target.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&source).unwrap(), b"original video bytes");
        let decoded = image::load_from_memory(&std::fs::read(&target).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (1, 1));
        assert_eq!(decoded.into_raw(), vec![240, 20, 4, 255]);
        assert!(snapshot
            .prepare_new_png(&directory.0.join("frame.jpg"), &cancel)
            .is_err());
        assert!(snapshot.prepare_new_png(&source, &cancel).is_err());
    }

    #[test]
    fn stream_and_nul_targets_are_rejected_before_staging() {
        let directory = TestDirectory::new();
        let source = directory.0.join("source.mp4");
        std::fs::write(&source, b"original video bytes").unwrap();
        let snapshot = snapshot(source.clone());
        let cancel = AtomicBool::new(false);
        for name in ["source.mp4:frame.png", "frame\0.png"] {
            let error = snapshot
                .prepare_new_png(&directory.0.join(name), &cancel)
                .err()
                .expect("无效目标不得进入暂存流程");
            assert_eq!(
                error.to_string(),
                tr!(
                    "视频帧保存文件名无效",
                    "The video frame file name is invalid"
                )
            );
            assert_eq!(std::fs::read(&source).unwrap(), b"original video bytes");
            assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
        }
    }

    #[test]
    #[ignore = "显式运行系统H.264解码帧到PNG的集成检查，不打开窗口或播放声音"]
    fn native_decoded_frame_png_round_trip() -> Result<()> {
        use crate::media_foundation::{MfRuntime, Mp4Writer, VideoReader};

        let _runtime = MfRuntime::new()?;
        let directory = TestDirectory::new();
        let source = directory.0.join("片段 sample.mp4");
        let (width, height) = (320_u32, 240_u32);
        let mut pixels = vec![0_u8; (width * height * 4) as usize];
        for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            let x = index % width as usize;
            pixel.copy_from_slice(if x < width as usize / 2 {
                &[0, 0, 255, 255]
            } else {
                &[255, 0, 0, 255]
            });
        }
        let mut writer = Mp4Writer::new(&source, width, height, 10, 600_000, false)?;
        for index in 0..6 {
            writer.write_video(
                &pixels,
                width,
                Duration::from_millis(index * 100),
                Duration::from_millis(100),
            )?;
        }
        writer.finish()?;
        let original = std::fs::read(&source)?;
        let mut reader = VideoReader::open(&source)?;
        assert_eq!((reader.width, reader.height), (width, height));
        assert!(reader.next()?.is_some());
        let decoded = reader.next()?.ok_or("合成视频缺少第二帧")?;
        assert_eq!(decoded.time, Duration::from_millis(100));
        assert_eq!(decoded.bgra.len(), pixels.len());
        drop(reader);

        // 与查看器共用BGRA存储约定；快照绑定这帧的真实PTS。
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::from_raw(width, height, decoded.bgra.clone())
                .ok_or("解码帧尺寸不完整")?,
        )]));
        let snapshot = FrameSnapshot::new(image, source.clone(), decoded.time)?;
        assert_eq!(snapshot.position, decoded.time);
        assert_eq!(
            snapshot.suggested_name(),
            format!("片段 sample_{}ms.png", decoded.time.as_millis())
        );
        let cancel = AtomicBool::new(false);
        let prepared_pixels = snapshot.prepare_pixels(&cancel)?;
        assert_eq!(
            (prepared_pixels.width, prepared_pixels.height),
            (width, height)
        );
        assert_eq!(prepared_pixels.bgra, decoded.bgra);

        let target = directory.0.join(snapshot.suggested_name());
        let published = snapshot.save_new_png(&target, &cancel)?;
        // Windows长路径前缀不改变目标，比较已创建文件的规范路径。
        assert_eq!(
            std::fs::canonicalize(&published)?,
            std::fs::canonicalize(&target)?
        );
        let saved = image::open(&target)?.to_rgba8();
        assert_eq!(saved.dimensions(), (width, height));
        for (saved, decoded) in saved
            .as_raw()
            .chunks_exact(4)
            .zip(decoded.bgra.chunks_exact(4))
        {
            assert_eq!(saved, [decoded[2], decoded[1], decoded[0], decoded[3]]);
        }
        // 容忍H.264量化，只检查远离分界的颜色方向；PNG本身须逐像素无损。
        let red = saved.get_pixel(width / 4, height / 2).0;
        let blue = saved.get_pixel(width * 3 / 4, height / 2).0;
        assert!(red[0] > 180 && red[1] < 80 && red[2] < 80);
        assert!(blue[2] > 180 && blue[0] < 80 && blue[1] < 80);
        assert_eq!(std::fs::read(&source)?, original);
        assert_eq!(std::fs::read_dir(&directory.0)?.count(), 2);
        Ok(())
    }

    #[test]
    fn existing_and_racing_targets_are_never_overwritten() {
        let directory = TestDirectory::new();
        let source = directory.0.join("source.png");
        let target = directory.0.join("frame.PNG");
        std::fs::write(&source, b"source bytes").unwrap();
        let snapshot = snapshot(source.clone());
        let cancel = AtomicBool::new(false);
        assert!(snapshot.prepare_new_png(&source, &cancel).is_err());
        assert_eq!(std::fs::read(&source).unwrap(), b"source bytes");
        let prepared = snapshot.prepare_new_png(&target, &cancel).unwrap();
        let staged = prepared.staged.clone().unwrap();
        std::fs::write(&target, b"another writer's output").unwrap();
        assert!(prepared.commit(&cancel).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"another writer's output");
        assert!(!staged.exists());
        assert!(snapshot.prepare_new_png(&target, &cancel).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"another writer's output");
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 2);
    }

    #[test]
    fn canceled_encoding_writer_stops_writing_and_flushing() {
        let cancel = AtomicBool::new(false);
        let mut writer = CancelWriter {
            writer: Vec::new(),
            cancel: &cancel,
        };
        writer.write_all(b"first").unwrap();
        cancel.store(true, Ordering::Release);
        let error = writer.write_all(b"second").unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::Interrupted);
        assert!(writer.flush().is_err());
        assert_eq!(writer.writer, b"first");
    }

    #[test]
    fn png_encoding_keeps_pixels_and_returns_cancel_write_and_flush_errors_without_panicking() {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Failure {
            Cancel,
            Write,
            Flush,
        }

        struct FaultOutput<'a> {
            bytes: Vec<u8>,
            fail_after: usize,
            failure: Failure,
            cancel: &'a AtomicBool,
        }

        impl Write for FaultOutput<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.failure == Failure::Flush {
                    self.bytes.extend_from_slice(bytes);
                    return Ok(bytes.len());
                }
                let remaining = self.fail_after.saturating_sub(self.bytes.len());
                if remaining == 0 {
                    return Err(io::Error::other("forced frame PNG write error"));
                }
                let count = remaining.min(bytes.len());
                self.bytes.extend_from_slice(&bytes[..count]);
                if self.failure == Failure::Cancel && self.bytes.len() == self.fail_after {
                    self.cancel.store(true, Ordering::Release);
                }
                Ok(count)
            }

            fn flush(&mut self) -> io::Result<()> {
                if self.failure == Failure::Flush {
                    Err(io::Error::other("forced frame PNG flush error"))
                } else {
                    Ok(())
                }
            }
        }

        let (width, height) = (64_u32, 32_u32);
        let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..height {
            for x in 0..width {
                bgra.extend_from_slice(if x < width / 2 {
                    &[20, 80, 220, 255]
                } else {
                    &[180, 60, 40, 128]
                });
            }
        }
        let mut encoded = Vec::new();
        encode_frame_png(&mut encoded, width, height, &bgra, &AtomicBool::new(false)).unwrap();
        assert!(encoded.len() < bgra.len() / 8, "重复像素仍应使用压缩");
        let decoded = image::load_from_memory(&encoded).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (width, height));
        for (actual, source) in decoded.as_raw().chunks_exact(4).zip(bgra.chunks_exact(4)) {
            assert_eq!(actual, [source[2], source[1], source[0], source[3]]);
        }

        // 从有效PNG的真实IDAT定位故障点：头已成功写出，压缩数据仅写出一个字节。
        let mut chunk = 8;
        let fail_after = loop {
            let length = u32::from_be_bytes(encoded[chunk..chunk + 4].try_into().unwrap()) as usize;
            if &encoded[chunk + 4..chunk + 8] == b"IDAT" {
                assert!(length > 1);
                break chunk + 9;
            }
            chunk += length + 12;
            assert!(chunk + 8 <= encoded.len(), "PNG必须包含IDAT");
        };
        for failure in [Failure::Cancel, Failure::Write, Failure::Flush] {
            let cancel = AtomicBool::new(false);
            let mut output = FaultOutput {
                bytes: Vec::new(),
                fail_after,
                failure,
                cancel: &cancel,
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                encode_frame_png(&mut output, width, height, &bgra, &cancel)
            }));
            assert!(result.is_ok(), "真实PNG编码不应因{failure:?}发生panic");
            assert!(result.unwrap().is_err(), "{failure:?}必须作为错误返回");
            if failure != Failure::Flush {
                assert_eq!(output.bytes.len(), fail_after);
            }
            assert_eq!(cancel.load(Ordering::Acquire), failure == Failure::Cancel);
        }
    }
}
