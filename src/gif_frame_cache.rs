//! 只解码一次的GIF中间缓存。64MiB限制仅计算u16像素key payload，帧/解码器等另计。
//! 超预算顺序写入本任务独占临时文件；重复key就地无损压缩，随机帧不膨胀。
//! 记录为little-endian，不暴露为最终GIF；短片内存路径不增加压缩扫描。
use crate::Result;
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) const KEY_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
static NEXT_CACHE: AtomicU64 = AtomicU64::new(0);

pub(super) struct CachedFrame {
    pub time: Duration,
    pub keys: Vec<u16>,
}

enum Storage {
    Memory(VecDeque<CachedFrame>),
    Disk(OwnedSpool),
}

pub(super) struct FrameCache {
    target: PathBuf,
    pixels: usize,
    limit: usize,
    storage: Storage,
    payload: usize,
    pub payload_peak: usize,
    pub disk_bytes: u64,
    count: u64,
    remaining: Option<u64>,
}

impl FrameCache {
    pub(super) fn new(target: &Path, pixels: usize, limit: usize) -> Result<Self> {
        if pixels == 0 || u32::try_from(pixels).is_err() || pixels.checked_mul(2).is_none() {
            return Err("GIF缓存像素数量无效".into());
        }
        Ok(Self {
            target: target.into(),
            pixels,
            limit,
            storage: Storage::Memory(VecDeque::new()),
            payload: 0,
            payload_peak: 0,
            disk_bytes: 0,
            count: 0,
            remaining: None,
        })
    }

    pub(super) fn push(
        &mut self,
        mut frame: CachedFrame,
        proceed: &mut dyn FnMut() -> bool,
    ) -> Result<()> {
        if self.remaining.is_some() || frame.keys.len() != self.pixels {
            return Err("GIF缓存写入状态或帧尺寸无效".into());
        }
        if !proceed() {
            return Err("已取消 GIF 转换".into());
        }
        let bytes = frame.keys.len() * 2;
        if matches!(self.storage, Storage::Memory(_))
            && self
                .payload
                .checked_add(bytes)
                .is_none_or(|sum| sum > self.limit)
        {
            let mut spool = OwnedSpool::new(&self.target)?;
            let prior = std::mem::replace(&mut self.storage, Storage::Memory(VecDeque::new()));
            if let Storage::Memory(frames) = prior {
                for mut prior in frames {
                    if !proceed() {
                        return Err("已取消 GIF 转换".into());
                    }
                    self.disk_bytes += spool.write_frame(&mut prior, proceed)?;
                }
            }
            self.payload = 0;
            self.storage = Storage::Disk(spool);
        }
        match &mut self.storage {
            Storage::Memory(frames) => {
                self.payload += bytes;
                self.payload_peak = self.payload_peak.max(self.payload);
                frames.push_back(frame);
            }
            Storage::Disk(spool) => {
                if !proceed() {
                    return Err("已取消 GIF 转换".into());
                }
                self.disk_bytes += spool.write_frame(&mut frame, proceed)?;
            }
        }
        self.count = self.count.checked_add(1).ok_or("GIF缓存帧计数溢出")?;
        Ok(())
    }

    pub(super) fn begin_read(&mut self) -> Result<()> {
        if self.remaining.is_some() {
            return Err("GIF缓存已开始读取".into());
        }
        if let Storage::Disk(spool) = &mut self.storage {
            spool.file.as_mut().unwrap().seek(SeekFrom::Start(0))?;
        }
        self.remaining = Some(self.count);
        Ok(())
    }

    pub(super) fn next(
        &mut self,
        proceed: &mut dyn FnMut() -> bool,
    ) -> Result<Option<CachedFrame>> {
        if !proceed() {
            return Err("已取消 GIF 转换".into());
        }
        let remaining = self.remaining.ok_or("GIF缓存尚未开始读取")?;
        if remaining == 0 {
            return Ok(None);
        }
        let frame = match &mut self.storage {
            Storage::Memory(frames) => {
                let frame = frames.pop_front().ok_or("GIF内存缓存记录缺失")?;
                self.payload -= frame.keys.len() * 2;
                frame
            }
            Storage::Disk(spool) => spool.read_frame(self.pixels, proceed)?,
        };
        self.remaining = Some(remaining - 1);
        Ok(Some(frame))
    }
}

struct OwnedSpool {
    // Drop先关闭句柄，再只删除create_new取得的这个文件。
    file: Option<File>,
    path: PathBuf,
}

impl OwnedSpool {
    fn new(target: &Path) -> io::Result<Self> {
        let parent = target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..128 {
            let path = parent.join(format!(
                ".rustshot-gif-keys-{}-{stamp}-{}.cache",
                std::process::id(),
                NEXT_CACHE.fetch_add(1, Ordering::Relaxed),
            ));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        path,
                    })
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "无法创建独占GIF像素缓存",
        ))
    }

    fn write_frame(
        &mut self,
        frame: &mut CachedFrame,
        proceed: &mut dyn FnMut() -> bool,
    ) -> io::Result<u64> {
        let len = u32::try_from(frame.keys.len()).map_err(|_| invalid("GIF缓存帧过大"))?;
        // 只压缩已消费的帧；输出不会超过输入，复用原Vec且不分配压缩工作缓冲。
        pack_keys(&mut frame.keys, proceed)?;
        let stored = u32::try_from(frame.keys.len()).map_err(|_| invalid("GIF缓存记录过大"))?;
        let file = self.file.as_mut().unwrap();
        let mut header = [0_u8; 20];
        header[..8].copy_from_slice(&frame.time.as_secs().to_le_bytes());
        header[8..12].copy_from_slice(&frame.time.subsec_nanos().to_le_bytes());
        header[12..16].copy_from_slice(&len.to_le_bytes());
        header[16..].copy_from_slice(&stored.to_le_bytes());
        check(proceed)?;
        file.write_all(&header)?;
        #[cfg(target_endian = "little")]
        {
            // SAFETY: u16无padding，所有位模式有效；只在immutable借用期间读取其bytes。
            // 文件明确little-endian，与本平台匹配；slice长度来自已分配Vec而非文件内容。
            let bytes = unsafe {
                std::slice::from_raw_parts(frame.keys.as_ptr().cast::<u8>(), frame.keys.len() * 2)
            };
            for bytes in bytes.chunks(IO_CHUNK) {
                check(proceed)?;
                file.write_all(bytes)?;
            }
        }
        #[cfg(target_endian = "big")]
        for (index, key) in frame.keys.iter().enumerate() {
            if index % (IO_CHUNK / 2) == 0 {
                check(proceed)?;
            }
            file.write_all(&key.to_le_bytes())?;
        }
        Ok(20 + u64::from(stored) * 2)
    }

    fn read_frame(
        &mut self,
        pixels: usize,
        proceed: &mut dyn FnMut() -> bool,
    ) -> io::Result<CachedFrame> {
        let file = self.file.as_mut().unwrap();
        let mut header = [0_u8; 20];
        check(proceed)?;
        file.read_exact(&mut header)?;
        let secs = u64::from_le_bytes(header[..8].try_into().unwrap());
        let nanos = u32::from_le_bytes(header[8..12].try_into().unwrap());
        let len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        let stored = u32::from_le_bytes(header[16..].try_into().unwrap()) as usize;
        if nanos >= 1_000_000_000 || len != pixels || stored == 0 || stored > pixels {
            return Err(invalid("GIF缓存记录尺寸或时间无效"));
        }
        let mut keys = vec![0_u16; pixels];
        #[cfg(target_endian = "little")]
        {
            // SAFETY: initialized u16存储可被任意byte写入，所有位模式有效；该独占借用
            // 完成后才读取keys。read_exact短读会报错，尚未填完的数据不会用于编码。
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(keys.as_mut_ptr().cast::<u8>(), stored * 2)
            };
            for bytes in bytes.chunks_mut(IO_CHUNK) {
                check(proceed)?;
                file.read_exact(bytes)?;
            }
        }
        #[cfg(target_endian = "big")]
        for (index, key) in keys[..stored].iter_mut().enumerate() {
            if index % (IO_CHUNK / 2) == 0 {
                check(proceed)?;
            }
            let mut bytes = [0_u8; 2];
            file.read_exact(&mut bytes)?;
            *key = u16::from_le_bytes(bytes);
        }
        unpack_keys(&mut keys, stored, proceed)?;
        Ok(CachedFrame {
            time: Duration::new(secs, nanos),
            keys,
        })
    }
}

const RUN_FLAG: u16 = 0x8000;
const RUN_MAX: usize = 0x7fff;

/// RGB555的最高位恒为零；带最高位的word表示后一个key重复3..32767次。
/// 短重复段直接保留原keys。每个重复段至少省一个word，因此可原地前向写入。
fn pack_keys(keys: &mut Vec<u16>, proceed: &mut dyn FnMut() -> bool) -> io::Result<()> {
    let mut read = 0;
    let mut write = 0;
    let mut next_check = 0;
    while read < keys.len() {
        if read >= next_check {
            check(proceed)?;
            next_check = read.saturating_add(IO_CHUNK / 2);
        }
        let key = keys[read];
        if key & RUN_FLAG != 0 {
            return Err(invalid("GIF缓存含无效像素索引"));
        }
        let mut end = read + 1;
        while end < keys.len() && keys[end] == key {
            if end >= next_check {
                check(proceed)?;
                next_check = end.saturating_add(IO_CHUNK / 2);
            }
            end += 1;
        }
        let mut count = end - read;
        while count >= 3 {
            let run = count.min(RUN_MAX);
            keys[write] = RUN_FLAG | run as u16;
            keys[write + 1] = key;
            write += 2;
            count -= run;
        }
        if count != 0 {
            keys[write..write + count].fill(key);
            write += count;
        }
        read = end;
    }
    keys.truncate(write);
    Ok(())
}

/// 先验证完整记录及展开长度，再从后向前原地恢复。原始随机帧保持旧读取路径。
fn unpack_keys(
    keys: &mut [u16],
    stored: usize,
    proceed: &mut dyn FnMut() -> bool,
) -> io::Result<()> {
    if stored == keys.len() {
        if !rustshot_gif_codec::valid_keys(keys) {
            return Err(invalid("GIF缓存含无效像素索引"));
        }
        return Ok(());
    }
    let mut position = 0;
    let mut expanded = 0_usize;
    let mut next_check = 0;
    while position < stored {
        if position >= next_check {
            check(proceed)?;
            next_check = position.saturating_add(IO_CHUNK / 2);
        }
        let word = keys[position];
        let run = if word & RUN_FLAG != 0 {
            let run = usize::from(word & !RUN_FLAG);
            if run < 3 || position + 1 >= stored || keys[position + 1] & RUN_FLAG != 0 {
                return Err(invalid("GIF缓存重复段无效"));
            }
            position += 2;
            run
        } else {
            position += 1;
            1
        };
        expanded = expanded
            .checked_add(run)
            .filter(|&n| n <= keys.len())
            .ok_or_else(|| invalid("GIF缓存展开尺寸无效"))?;
    }
    if expanded != keys.len() {
        return Err(invalid("GIF缓存展开尺寸无效"));
    }
    let mut read = stored;
    let mut write = keys.len();
    let mut checked_words = 0;
    while read != 0 {
        if checked_words >= IO_CHUNK / 2 {
            check(proceed)?;
            checked_words = 0;
        }
        let key = keys[read - 1];
        let (run, words) = if read >= 2 && keys[read - 2] & RUN_FLAG != 0 {
            (usize::from(keys[read - 2] & !RUN_FLAG), 2)
        } else {
            (1, 1)
        };
        keys[write - run..write].fill(key);
        write -= run;
        read -= words;
        checked_words += run;
    }
    check(proceed)?;
    Ok(())
}

impl Drop for OwnedSpool {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Err(error) = fs::remove_file(&self.path) {
            eprintln!("GIF自有像素缓存未能清理 {}：{error}", self.path.display());
        }
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
const IO_CHUNK: usize = 256 * 1024;
fn check(proceed: &mut dyn FnMut() -> bool) -> io::Result<()> {
    if proceed() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "已取消 GIF 转换",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Case(PathBuf);
    impl Case {
        fn new() -> Self {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/qa/cache-models")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT_CACHE.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("existing.gif"), b"existing-output").unwrap();
            Self(root)
        }
        fn target(&self) -> PathBuf {
            self.0.join("result.gif")
        }
        fn preserved(&self) {
            assert_eq!(
                fs::read(self.0.join("existing.gif")).unwrap(),
                b"existing-output"
            );
            assert!(!self.target().exists());
        }
        fn caches(&self) -> usize {
            fs::read_dir(&self.0)
                .unwrap()
                .filter_map(|p| p.ok())
                .filter(|p| p.path().extension().is_some_and(|e| e == "cache"))
                .count()
        }
    }
    fn frame(n: u16) -> CachedFrame {
        CachedFrame {
            time: Duration::new(7, u32::from(n) * 100),
            keys: vec![n, 1, 31, 1024, 32767],
        }
    }
    #[test]
    fn gif_cache_memory_and_spill_keep_all_keys_pts_and_owned_cleanup() {
        for limit in [0, 10, 20, 64] {
            let case = Case::new();
            let mut cache = FrameCache::new(&case.target(), 5, limit).unwrap();
            for n in 0..4 {
                cache.push(frame(n), &mut || true).unwrap();
            }
            assert!(cache.payload_peak <= limit);
            assert_eq!(cache.disk_bytes > 0, limit < 40);
            cache.begin_read().unwrap();
            for n in 0..4 {
                let actual = cache.next(&mut || true).unwrap().unwrap();
                let expected = frame(n);
                assert_eq!(actual.time, expected.time);
                assert_eq!(actual.keys, expected.keys);
            }
            assert!(cache.next(&mut || true).unwrap().is_none());
            drop(cache);
            assert_eq!(case.caches(), 0);
            case.preserved();
        }
    }
    #[test]
    fn gif_cache_cancel_during_spill_cleans_only_its_file() {
        let case = Case::new();
        let mut cache = FrameCache::new(&case.target(), 5, 30).unwrap();
        for n in 0..3 {
            cache.push(frame(n), &mut || true).unwrap();
        }
        let mut checks = 0;
        let error = cache
            .push(frame(3), &mut || {
                checks += 1;
                checks < 3
            })
            .err()
            .unwrap();
        assert!(error.to_string().contains("取消"));
        drop(cache);
        assert_eq!(case.caches(), 0);
        case.preserved();
    }
    #[test]
    fn gif_cache_truncated_and_invalid_disk_data_return_error() {
        for mutation in 0..3 {
            let case = Case::new();
            let mut cache = FrameCache::new(&case.target(), 5, 0).unwrap();
            cache.push(frame(3), &mut || true).unwrap();
            let Storage::Disk(spool) = &cache.storage else {
                panic!("disk expected")
            };
            let mut modify = OpenOptions::new().write(true).open(&spool.path).unwrap();
            match mutation {
                0 => modify.set_len(18).unwrap(),
                1 => {
                    modify.seek(SeekFrom::Start(12)).unwrap();
                    modify.write_all(&6_u32.to_le_bytes()).unwrap();
                }
                _ => {
                    modify.seek(SeekFrom::Start(16)).unwrap();
                    modify.write_all(&u16::MAX.to_le_bytes()).unwrap();
                }
            }
            drop(modify);
            cache.begin_read().unwrap();
            assert!(cache.next(&mut || true).is_err());
            drop(cache);
            assert_eq!(case.caches(), 0);
            case.preserved();
        }
    }
    #[test]
    fn gif_cache_rejects_wrong_shape_and_write_after_read() {
        let case = Case::new();
        assert!(FrameCache::new(&case.target(), 0, 0).is_err());
        assert!(FrameCache::new(&case.target(), usize::MAX, 0).is_err());
        let mut cache = FrameCache::new(&case.target(), 6, 64).unwrap();
        assert!(cache.push(frame(0), &mut || true).is_err());
        cache.begin_read().unwrap();
        assert!(cache.push(frame(0), &mut || true).is_err());
        assert!(cache.begin_read().is_err());
        assert!(cache.next(&mut || true).unwrap().is_none());
        case.preserved();
    }
    #[test]
    fn gif_cache_two_spilled_sessions_keep_separate_ownership() {
        let case = Case::new();
        let mut first = FrameCache::new(&case.target(), 5, 0).unwrap();
        let mut second = FrameCache::new(&case.target(), 5, 0).unwrap();
        first.push(frame(3), &mut || true).unwrap();
        second.push(frame(7), &mut || true).unwrap();
        assert_eq!(case.caches(), 2);
        drop(first);
        assert_eq!(case.caches(), 1);
        second.begin_read().unwrap();
        let actual = second.next(&mut || true).unwrap().unwrap();
        assert_eq!(actual.keys, frame(7).keys);
        assert_eq!(actual.time, frame(7).time);
        drop(second);
        assert_eq!(case.caches(), 0);
        case.preserved();
    }
    #[test]
    fn gif_cache_cancel_in_real_write_and_read_chunks_never_yields_partial_frame() {
        let pixels = IO_CHUNK * 2;
        for writing in [true, false] {
            let case = Case::new();
            let mut cache = FrameCache::new(&case.target(), pixels, 0).unwrap();
            let value = CachedFrame {
                time: Duration::from_millis(73),
                // 合法无重复的key确保仍覆盖真实多块文件I/O；压缩检查单独验证。
                keys: (0..pixels).map(|n| (n % 32768) as u16).collect(),
            };
            let mut checks = 0;
            if writing {
                assert!(cache
                    .push(value, &mut || {
                        checks += 1;
                        checks < 9
                    })
                    .is_err());
                let Storage::Disk(spool) = &cache.storage else {
                    panic!("disk expected")
                };
                let bytes = fs::metadata(&spool.path).unwrap().len();
                assert!(bytes > 20 && bytes < 20 + (pixels * 2) as u64);
            } else {
                cache.push(value, &mut || true).unwrap();
                cache.begin_read().unwrap();
                assert!(cache
                    .next(&mut || {
                        checks += 1;
                        checks < 4
                    })
                    .is_err());
            }
            drop(cache);
            assert_eq!(case.caches(), 0);
            case.preserved();
        }
    }

    #[test]
    fn gif_cache_run_codec_is_in_place_exact_and_never_larger_than_raw() {
        let cases = [
            vec![0],
            vec![1, 1, 2, 2, 3, 3],
            vec![32767; RUN_MAX * 2 + 2],
            (0..65536).map(|n| (n % 32768) as u16).collect(),
            (0..65536)
                .map(|n| {
                    (((n as u32).wrapping_mul(1664525).wrapping_add(1013904223) >> 8) & 32767)
                        as u16
                })
                .collect(),
            (0..1024)
                .flat_map(|n| std::iter::repeat_n((n % 32768) as u16, 97))
                .collect(),
            vec![4, 4, 4, 5, 6, 6, 7, 7, 7, 7, 8],
        ];
        for expected in cases {
            let mut keys = expected.clone();
            let pointer = keys.as_ptr();
            let capacity = keys.capacity();
            pack_keys(&mut keys, &mut || true).unwrap();
            assert!(keys.len() <= expected.len());
            assert_eq!(keys.as_ptr(), pointer);
            assert_eq!(keys.capacity(), capacity);
            let stored = keys.len();
            keys.resize(expected.len(), 0);
            unpack_keys(&mut keys, stored, &mut || true).unwrap();
            assert_eq!(keys.as_ptr(), pointer);
            assert_eq!(keys, expected);
        }
    }

    #[test]
    fn gif_cache_spilled_runs_keep_pts_keys_and_reduce_real_file_payload() {
        let case = Case::new();
        let pixels = RUN_MAX * 2 + 11;
        let mut cache = FrameCache::new(&case.target(), pixels, pixels * 2).unwrap();
        for n in 0..3 {
            cache
                .push(
                    CachedFrame {
                        time: Duration::new(11, n * 123_456_789),
                        keys: vec![n as u16; pixels],
                    },
                    &mut || true,
                )
                .unwrap();
        }
        assert_eq!(cache.payload_peak, pixels * 2);
        assert!(cache.disk_bytes < 128);
        cache.begin_read().unwrap();
        for n in 0..3 {
            let actual = cache.next(&mut || true).unwrap().unwrap();
            assert_eq!(actual.time, Duration::new(11, n * 123_456_789));
            assert_eq!(actual.keys, vec![n as u16; pixels]);
        }
        assert!(cache.next(&mut || true).unwrap().is_none());
        drop(cache);
        assert_eq!(case.caches(), 0);
        case.preserved();
    }

    #[test]
    fn gif_cache_run_codec_rejects_malformed_runs_without_partial_frame() {
        for encoded in [
            vec![RUN_FLAG, 1],
            vec![RUN_FLAG | 2, 1],
            vec![RUN_FLAG | 5],
            vec![RUN_FLAG | 5, RUN_FLAG],
            vec![RUN_FLAG | 4, 1],
            vec![RUN_FLAG | 6, 1],
        ] {
            let stored = encoded.len();
            let mut keys = encoded;
            keys.resize(5, 0);
            assert!(unpack_keys(&mut keys, stored, &mut || true).is_err());
        }
        let mut keys = vec![0, 32768, 1];
        assert!(pack_keys(&mut keys, &mut || true).is_err());
    }

    #[test]
    fn gif_cache_run_codec_cancels_bounded_scanning_and_expansion() {
        let mut keys = vec![1; IO_CHUNK * 2];
        let mut checks = 0;
        assert!(pack_keys(&mut keys, &mut || {
            checks += 1;
            checks < 3
        })
        .is_err());
        let expected = (0..8192)
            .flat_map(|n| std::iter::repeat_n((n % 32768) as u16, 97))
            .collect::<Vec<_>>();
        let mut keys = expected;
        let original = keys.len();
        pack_keys(&mut keys, &mut || true).unwrap();
        let stored = keys.len();
        keys.resize(original, 0);
        let mut checks = 0;
        assert!(unpack_keys(&mut keys, stored, &mut || {
            checks += 1;
            checks < 3
        })
        .is_err());
    }
}
