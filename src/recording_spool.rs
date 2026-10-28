//! 内部实验的单文件 top-down BGRA 中间源；只顺序写入/验证，不修复或删除产物。
//! 固定小头、逐帧 SHA-256 与封口仅用于完整性检查，不提供跨版本兼容或生产恢复合同。

use crate::Result;
use serde::Serialize;
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::Duration,
};
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;

const BLOCK_BYTES: usize = 64;
const FILE_MAGIC: &[u8; 8] = b"RBSP0001";
const FRAME_MAGIC: &[u8; 4] = b"FRM1";
const SEAL_MAGIC: &[u8; 8] = b"RBSE0001";
const MAX_SIDE: u32 = 4096;
const MAX_PIXELS: u64 = 4_194_304;
const MAX_FRAME_BYTES: u64 = 16 * 1024 * 1024;
const MAX_FRAMES: u32 = 240;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const TICKS_PER_SECOND: u64 = 10_000_000;
const SUMMARY_SCOPE: &str = "Internal experimental top-down BGRA spool, version 1 only. Frame hashes bind the file header, record metadata and pixels; the seal binds the validated frame-digest sequence and final totals. Writer summaries validate submitted frame contracts, successful writes/flush/sync and final handle length; reader summaries validate persisted bytes through EOF. SHA-256 is an integrity check, not authentication or a power-loss guarantee. Bytes are logical file length, not physical disk allocation. Windows handles share reads but deny concurrent writes and deletion. Reader state retains one in-progress frame; callers control retention of returned frames. No repair, export, deletion or native media processing is performed.";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SpoolSpec {
    pub width: u32,
    pub height: u32,
    /// 名义帧率，不要求每条 record 的 duration 完全相等。
    pub fps: u32,
    pub max_frames: u32,
    pub max_bytes: u64,
    pub frame_bytes: u64,
    /// 包含所有 max_frames 的 record 头、像素及文件头/封口。
    pub expected_max_bytes: u64,
}

impl SpoolSpec {
    pub(crate) fn new(
        width: u32,
        height: u32,
        fps: u32,
        max_frames: u32,
        max_bytes: u64,
    ) -> Result<Self> {
        if width == 0
            || height == 0
            || width > MAX_SIDE
            || height > MAX_SIDE
            || !(1..=240).contains(&fps)
            || !(1..=MAX_FRAMES).contains(&max_frames)
            || max_bytes == 0
            || max_bytes > MAX_FILE_BYTES
        {
            return Err("Spool specification exceeds its dimension, frame-rate, frame-count or file budget limits".into());
        }
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or("Spool pixel count overflow")?;
        let frame_bytes = pixels
            .checked_mul(4)
            .ok_or("Spool frame byte count overflow")?;
        if pixels > MAX_PIXELS || frame_bytes > MAX_FRAME_BYTES {
            return Err("Spool frame exceeds its 4-Mpixel / 16-MiB limit".into());
        }
        let record_bytes = frame_bytes
            .checked_add(BLOCK_BYTES as u64)
            .ok_or("Spool record size overflow")?;
        let expected_max_bytes = record_bytes
            .checked_mul(u64::from(max_frames))
            .and_then(|bytes| bytes.checked_add(2 * BLOCK_BYTES as u64))
            .ok_or("Spool maximum byte count overflow")?;
        if expected_max_bytes > max_bytes {
            return Err(
                "Spool maximum frames including headers and seal exceed the requested byte budget"
                    .into(),
            );
        }
        Ok(Self {
            width,
            height,
            fps,
            max_frames,
            max_bytes,
            frame_bytes,
            expected_max_bytes,
        })
    }

    fn checked(self) -> Result<Self> {
        let checked = Self::new(
            self.width,
            self.height,
            self.fps,
            self.max_frames,
            self.max_bytes,
        )?;
        if checked != self {
            return Err("Spool specification has inconsistent derived byte counts".into());
        }
        Ok(checked)
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SpoolSummary {
    pub spec: SpoolSpec,
    pub validated_frames: u32,
    pub sealed_complete: bool,
    /// Reader 为打开时的句柄长度，Writer seal 为同步后核对的实际长度。
    pub bytes: u64,
    /// 只包含已验证的完整 record；完整封口和 EOF 均通过后才包含 footer。
    pub validated_prefix_bytes: u64,
    pub payload_bytes: u64,
    pub timeline_end_100ns: u64,
    pub claimed_total_bytes: Option<u64>,
    pub claimed_total_bytes_verified: bool,
    pub terminal_error: Option<String>,
    pub scope: &'static str,
}

impl SpoolSummary {
    fn new(spec: SpoolSpec, bytes: u64) -> Self {
        Self {
            spec,
            validated_frames: 0,
            sealed_complete: false,
            bytes,
            validated_prefix_bytes: BLOCK_BYTES as u64,
            payload_bytes: 0,
            timeline_end_100ns: 0,
            claimed_total_bytes: None,
            claimed_total_bytes_verified: false,
            terminal_error: None,
            scope: SUMMARY_SCOPE,
        }
    }
}

pub(crate) struct SpoolFrame {
    pub index: u32,
    pub pts: Duration,
    pub duration: Duration,
    pub pixels: Vec<u8>,
}

fn put_u32(block: &mut [u8; BLOCK_BYTES], offset: usize, value: u32) {
    block[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(block: &mut [u8; BLOCK_BYTES], offset: usize, value: u64) {
    block[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}
fn get_u32(block: &[u8; BLOCK_BYTES], offset: usize) -> u32 {
    let mut bytes = [0; 4];
    bytes.copy_from_slice(&block[offset..offset + 4]);
    u32::from_le_bytes(bytes)
}
fn get_u64(block: &[u8; BLOCK_BYTES], offset: usize) -> u64 {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(&block[offset..offset + 8]);
    u64::from_le_bytes(bytes)
}

fn header(spec: SpoolSpec) -> [u8; BLOCK_BYTES] {
    let mut block = [0; BLOCK_BYTES];
    block[..8].copy_from_slice(FILE_MAGIC);
    put_u32(&mut block, 8, 1);
    put_u32(&mut block, 12, BLOCK_BYTES as u32);
    put_u32(&mut block, 16, spec.width);
    put_u32(&mut block, 20, spec.height);
    put_u32(&mut block, 24, spec.fps);
    put_u32(&mut block, 28, spec.max_frames);
    put_u64(&mut block, 32, spec.max_bytes);
    put_u64(&mut block, 40, spec.frame_bytes);
    put_u64(&mut block, 48, spec.expected_max_bytes);
    block
}

fn parse_header(block: &[u8; BLOCK_BYTES]) -> Result<SpoolSpec> {
    if &block[..8] != FILE_MAGIC
        || get_u32(block, 8) != 1
        || get_u32(block, 12) != BLOCK_BYTES as u32
        || block[56..].iter().any(|&byte| byte != 0)
    {
        return Err("Spool header magic, version, length or reserved bytes are invalid".into());
    }
    SpoolSpec {
        width: get_u32(block, 16),
        height: get_u32(block, 20),
        fps: get_u32(block, 24),
        max_frames: get_u32(block, 28),
        max_bytes: get_u64(block, 32),
        frame_bytes: get_u64(block, 40),
        expected_max_bytes: get_u64(block, 48),
    }
    .checked()
}

fn chain(header: &[u8; BLOCK_BYTES]) -> Sha256 {
    let mut hash = Sha256::new();
    hash.update(b"RBSP-SEAL-1");
    hash.update(header);
    hash
}

fn frame_digest(header: &[u8; BLOCK_BYTES], record: &[u8; BLOCK_BYTES], pixels: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"RBSP-FRAME-1");
    hash.update(header);
    hash.update(&record[..32]);
    hash.update(pixels);
    hash.finalize().into()
}

fn seal_digest(chain: &Sha256, seal: &[u8; BLOCK_BYTES]) -> [u8; 32] {
    let mut hash = chain.clone();
    hash.update(&seal[..32]);
    hash.finalize().into()
}

fn ticks(value: Duration) -> Result<u64> {
    if value.as_nanos() % 100 != 0 {
        return Err("Spool timestamps and durations must be aligned to 100 ns".into());
    }
    Ok(u64::try_from(value.as_nanos() / 100)?)
}
fn from_ticks(value: u64) -> Duration {
    Duration::new(
        value / TICKS_PER_SECOND,
        ((value % TICKS_PER_SECOND) * 100) as u32,
    )
}
fn timeline(pts: u64, duration: u64, expected: u64) -> Result<u64> {
    if pts != expected || duration == 0 {
        return Err(
            "Spool timeline must start at zero, remain contiguous and have positive durations"
                .into(),
        );
    }
    Ok(pts
        .checked_add(duration)
        .ok_or("Spool timeline exceeds its 64-bit media-tick range")?)
}

pub(crate) struct SpoolWriter {
    file: File,
    spec: SpoolSpec,
    header: [u8; BLOCK_BYTES],
    chain: Sha256,
    frames: u32,
    end: u64,
    bytes: u64,
    failed: bool,
}

impl SpoolWriter {
    pub(crate) fn create(path: &Path, spec: SpoolSpec) -> Result<Self> {
        let spec = spec.checked()?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(windows)]
        options.share_mode(FILE_SHARE_READ.0);
        let mut file = options.open(path)?;
        let header = header(spec);
        file.write_all(&header)?;
        Ok(Self {
            file,
            spec,
            header,
            chain: chain(&header),
            frames: 0,
            end: 0,
            bytes: BLOCK_BYTES as u64,
            failed: false,
        })
    }

    pub(crate) fn push(&mut self, pixels: &[u8], pts: Duration, duration: Duration) -> Result<()> {
        if self.failed {
            return Err("Spool writer is terminal after an I/O failure".into());
        }
        if self.frames >= self.spec.max_frames || pixels.len() as u64 != self.spec.frame_bytes {
            return Err("Spool frame count or exact BGRA byte length is invalid".into());
        }
        pts.checked_add(duration).ok_or("Spool duration overflow")?;
        let (pts, duration) = (ticks(pts)?, ticks(duration)?);
        let end = timeline(pts, duration, self.end)?;
        let bytes = self
            .bytes
            .checked_add(BLOCK_BYTES as u64)
            .and_then(|bytes| bytes.checked_add(self.spec.frame_bytes))
            .ok_or("Spool byte count overflow")?;
        if bytes
            .checked_add(BLOCK_BYTES as u64)
            .ok_or("Spool seal size overflow")?
            > self.spec.expected_max_bytes
        {
            return Err("Spool frame would exceed its preflight byte budget".into());
        }
        let mut record = [0; BLOCK_BYTES];
        record[..4].copy_from_slice(FRAME_MAGIC);
        put_u32(&mut record, 4, self.frames);
        put_u64(&mut record, 8, pts);
        put_u64(&mut record, 16, duration);
        put_u64(&mut record, 24, self.spec.frame_bytes);
        let digest = frame_digest(&self.header, &record, pixels);
        record[32..].copy_from_slice(&digest);
        let written: std::io::Result<()> = (|| {
            self.file.write_all(&record)?;
            self.file.write_all(pixels)
        })();
        if let Err(error) = written {
            self.failed = true;
            return Err(error.into());
        }
        self.chain.update(digest);
        self.frames += 1;
        self.end = end;
        self.bytes = bytes;
        Ok(())
    }

    pub(crate) fn seal(mut self) -> Result<SpoolSummary> {
        if self.failed || self.frames == 0 {
            return Err("An empty or I/O-failed spool cannot be sealed".into());
        }
        let total = self
            .bytes
            .checked_add(BLOCK_BYTES as u64)
            .ok_or("Spool seal byte count overflow")?;
        let mut seal = [0; BLOCK_BYTES];
        seal[..8].copy_from_slice(SEAL_MAGIC);
        put_u32(&mut seal, 8, self.frames);
        put_u64(&mut seal, 16, self.end);
        put_u64(&mut seal, 24, total);
        let digest = seal_digest(&self.chain, &seal);
        seal[32..].copy_from_slice(&digest);
        self.file.write_all(&seal)?;
        self.file.flush()?;
        self.file.sync_all()?;
        let actual = self.file.metadata()?.len();
        if actual != total || actual > self.spec.max_bytes || actual > self.spec.expected_max_bytes
        {
            return Err(
                "Sealed spool length does not match its actual handle length or budget".into(),
            );
        }
        let mut summary = SpoolSummary::new(self.spec, actual);
        summary.validated_frames = self.frames;
        summary.sealed_complete = true;
        summary.validated_prefix_bytes = actual;
        summary.payload_bytes = u64::from(self.frames) * self.spec.frame_bytes;
        summary.timeline_end_100ns = self.end;
        summary.claimed_total_bytes = Some(total);
        summary.claimed_total_bytes_verified = true;
        Ok(summary)
    }
}

pub(crate) struct SpoolReader {
    pub spec: SpoolSpec,
    checked_spec: SpoolSpec,
    file: File,
    header: [u8; BLOCK_BYTES],
    chain: Sha256,
    summary: SpoolSummary,
}

impl SpoolReader {
    pub(crate) fn open(path: &Path, max_bytes: u64) -> Result<Self> {
        if max_bytes == 0 || max_bytes > MAX_FILE_BYTES {
            return Err("Spool reader byte budget must be within 1..=256 MiB".into());
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        options.share_mode(FILE_SHARE_READ.0);
        let mut file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > max_bytes {
            return Err("Spool is not a regular file within the reader byte budget".into());
        }
        let mut header = [0; BLOCK_BYTES];
        file.read_exact(&mut header)?;
        let spec = parse_header(&header)?;
        Ok(Self {
            spec,
            checked_spec: spec,
            file,
            header,
            chain: chain(&header),
            summary: SpoolSummary::new(spec, metadata.len()),
        })
    }

    pub(crate) fn summary(&self) -> SpoolSummary {
        self.summary.clone()
    }

    /// None 只代表完整 seal 与真实 EOF 已验证；错误后重复调用仍返回终态错误。
    pub(crate) fn next(&mut self) -> Result<Option<SpoolFrame>> {
        if let Some(error) = &self.summary.terminal_error {
            return Err(format!("Spool reader remains terminal: {error}").into());
        }
        if self.summary.sealed_complete {
            return Ok(None);
        }
        let result = self.next_record();
        if let Err(error) = &result {
            self.summary.terminal_error = Some(error.to_string().chars().take(512).collect());
        }
        result
    }

    fn next_record(&mut self) -> Result<Option<SpoolFrame>> {
        if self.spec != self.checked_spec {
            return Err("Spool reader specification was changed after opening".into());
        }
        let mut block = [0; BLOCK_BYTES];
        self.file.read_exact(&mut block)?;
        if &block[..8] == SEAL_MAGIC {
            return self.read_seal(&block);
        }
        if &block[..4] != FRAME_MAGIC
            || self.summary.validated_frames >= self.checked_spec.max_frames
            || get_u32(&block, 4) != self.summary.validated_frames
            || get_u64(&block, 24) != self.checked_spec.frame_bytes
        {
            return Err(
                "Spool record marker, index, payload length or frame limit is invalid".into(),
            );
        }
        let (pts, duration) = (get_u64(&block, 8), get_u64(&block, 16));
        let end = timeline(pts, duration, self.summary.timeline_end_100ns)?;
        let prefix = self
            .summary
            .validated_prefix_bytes
            .checked_add(BLOCK_BYTES as u64)
            .and_then(|bytes| bytes.checked_add(self.checked_spec.frame_bytes))
            .ok_or("Spool record byte count overflow")?;
        if prefix > self.summary.bytes
            || prefix
                .checked_add(BLOCK_BYTES as u64)
                .ok_or("Spool seal length overflow")?
                > self.checked_spec.expected_max_bytes
        {
            return Err("Spool frame is truncated or exceeds its declared file budget".into());
        }
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(self.checked_spec.frame_bytes as usize)?;
        pixels.resize(self.checked_spec.frame_bytes as usize, 0);
        self.file.read_exact(&mut pixels)?;
        let digest = frame_digest(&self.header, &block, &pixels);
        if block[32..] != digest {
            return Err("Spool frame SHA-256 does not match its metadata and pixels".into());
        }
        let index = self.summary.validated_frames;
        self.chain.update(digest);
        self.summary.validated_frames += 1;
        self.summary.validated_prefix_bytes = prefix;
        self.summary.payload_bytes += self.checked_spec.frame_bytes;
        self.summary.timeline_end_100ns = end;
        Ok(Some(SpoolFrame {
            index,
            pts: from_ticks(pts),
            duration: from_ticks(duration),
            pixels,
        }))
    }

    fn read_seal(&mut self, seal: &[u8; BLOCK_BYTES]) -> Result<Option<SpoolFrame>> {
        let total = get_u64(seal, 24);
        self.summary.claimed_total_bytes = Some(total);
        let expected_total = self
            .summary
            .validated_prefix_bytes
            .checked_add(BLOCK_BYTES as u64)
            .ok_or("Spool total byte count overflow")?;
        if self.summary.validated_frames == 0
            || seal[12..16].iter().any(|&byte| byte != 0)
            || get_u32(seal, 8) != self.summary.validated_frames
            || get_u64(seal, 16) != self.summary.timeline_end_100ns
            || total != expected_total
            || total > self.checked_spec.max_bytes
            || total > self.checked_spec.expected_max_bytes
            || seal[32..] != seal_digest(&self.chain, seal)
        {
            return Err(
                "Spool seal count, timeline, byte totals, reserved bytes or SHA-256 are invalid"
                    .into(),
            );
        }
        let mut extra = [0; 1];
        if self.file.read(&mut extra)? != 0
            || total != self.summary.bytes
            || self.file.metadata()?.len() != total
        {
            return Err(
                "Spool seal is followed by trailing bytes or the file length changed".into(),
            );
        }
        self.summary.sealed_complete = true;
        self.summary.claimed_total_bytes_verified = true;
        self.summary.validated_prefix_bytes = total;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture {
        directory: PathBuf,
        path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            static SERIAL: AtomicU64 = AtomicU64::new(0);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "rustshot-spool-{}-{stamp}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            Self {
                path: directory.join("sample.rbs"),
                directory,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
            let _ = fs::remove_dir(&self.directory);
        }
    }
    fn three_frames(path: &Path) -> SpoolSummary {
        let spec = SpoolSpec::new(2, 2, 30, 4, 1024).unwrap();
        let mut writer = SpoolWriter::create(path, spec).unwrap();
        let mut pts = 0;
        for (index, duration) in [333_333, 333_334, 333_333].into_iter().enumerate() {
            writer
                .push(&[index as u8; 16], from_ticks(pts), from_ticks(duration))
                .unwrap();
            pts += duration;
        }
        writer.seal().unwrap()
    }

    #[test]
    fn recording_spool_valid_roundtrip_requires_footer_and_eof() {
        let fixture = Fixture::new();
        let sealed = three_frames(&fixture.path);
        assert!(sealed.sealed_complete && sealed.claimed_total_bytes_verified);
        assert!(sealed.validated_frames < sealed.spec.max_frames);
        assert_eq!(sealed.bytes, 64 + 3 * 80 + 64);
        let mut reader = SpoolReader::open(&fixture.path, 1024).unwrap();
        for index in 0..3 {
            let frame = reader.next().unwrap().unwrap();
            assert_eq!(frame.index, index);
            assert_eq!(frame.pixels, vec![index as u8; 16]);
            assert_eq!(frame.pts, from_ticks([0, 333_333, 666_667][index as usize]));
            assert_eq!(
                frame.duration,
                from_ticks([333_333, 333_334, 333_333][index as usize])
            );
            assert_eq!(reader.summary().validated_frames, index + 1);
            assert!(!reader.summary().sealed_complete);
        }
        assert!(reader.next().unwrap().is_none());
        assert!(reader.next().unwrap().is_none());
        let complete = reader.summary();
        assert!(complete.sealed_complete && complete.claimed_total_bytes_verified);
        assert_eq!(complete.claimed_total_bytes, Some(complete.bytes));
        assert_eq!(complete.validated_prefix_bytes, complete.bytes);
        assert_eq!(complete.timeline_end_100ns, 1_000_000);
    }

    #[test]
    fn recording_spool_damage_and_truncation_preserve_verified_prefix() {
        let fixture = Fixture::new();
        three_frames(&fixture.path);
        let original = fs::read(&fixture.path).unwrap();
        let mut corrupted = original.clone();
        corrupted[64 + 80 + 64] ^= 1;
        fs::write(&fixture.path, &corrupted).unwrap();
        {
            let mut reader = SpoolReader::open(&fixture.path, 1024).unwrap();
            assert!(reader.next().unwrap().is_some());
            assert!(reader.next().is_err());
            assert!(reader.next().is_err());
            let summary = reader.summary();
            assert_eq!(summary.validated_frames, 1);
            assert_eq!(summary.validated_prefix_bytes, 64 + 80);
            assert!(!summary.sealed_complete && summary.terminal_error.is_some());
        }
        fs::write(&fixture.path, &original[..64 + 2 * 80 + 64 + 8]).unwrap();
        let mut reader = SpoolReader::open(&fixture.path, 1024).unwrap();
        assert!(reader.next().unwrap().is_some());
        assert!(reader.next().unwrap().is_some());
        assert!(reader.next().is_err());
        assert_eq!(reader.summary().validated_frames, 2);
        assert!(!reader.summary().claimed_total_bytes_verified);
    }

    #[test]
    fn recording_spool_preflight_timeline_and_trailing_data_are_strict() {
        assert!(ticks(Duration::from_secs(u64::MAX)).is_err());
        assert!(timeline(u64::MAX, 1, u64::MAX).is_err());
        assert!(SpoolSpec::new(2, 2, 30, 2, 287).is_err());
        assert!(SpoolSpec::new(4097, 1, 30, 1, MAX_FILE_BYTES).is_err());
        assert!(SpoolSpec::new(4096, 1025, 30, 1, MAX_FILE_BYTES).is_err());
        assert!(SpoolSpec::new(2, 2, 0, 1, 1024).is_err());
        assert!(SpoolSpec::new(2, 2, 30, 241, 1024).is_err());
        assert!(SpoolSpec::new(2, 2, 30, 1, MAX_FILE_BYTES + 1).is_err());
        let fixture = Fixture::new();
        let spec = SpoolSpec::new(2, 2, 30, 2, 1024).unwrap();
        let mut writer = SpoolWriter::create(&fixture.path, spec).unwrap();
        assert!(SpoolWriter::create(&fixture.path, spec).is_err());
        assert!(writer
            .push(&[0; 15], Duration::ZERO, from_ticks(1))
            .is_err());
        assert!(writer.push(&[0; 16], from_ticks(1), from_ticks(1)).is_err());
        assert!(writer
            .push(&[0; 16], Duration::ZERO, Duration::ZERO)
            .is_err());
        assert!(writer
            .push(&[0; 16], Duration::from_nanos(1), from_ticks(1))
            .is_err());
        writer
            .push(&[0; 16], Duration::ZERO, from_ticks(1))
            .unwrap();
        assert!(writer
            .push(&[0; 16], Duration::ZERO, from_ticks(1))
            .is_err());
        assert!(writer.push(&[0; 16], from_ticks(2), from_ticks(1)).is_err());
        writer.push(&[1; 16], from_ticks(1), from_ticks(1)).unwrap();
        assert!(writer.push(&[0; 16], from_ticks(2), from_ticks(1)).is_err());
        writer.seal().unwrap();
        OpenOptions::new()
            .append(true)
            .open(&fixture.path)
            .unwrap()
            .write_all(&[9])
            .unwrap();
        let mut reader = SpoolReader::open(&fixture.path, 1024).unwrap();
        assert!(reader.next().unwrap().is_some());
        assert!(reader.next().unwrap().is_some());
        assert!(reader.next().is_err());
        let summary = reader.summary();
        assert_eq!(summary.validated_frames, 2);
        assert_eq!(summary.claimed_total_bytes, Some(288));
        assert!(!summary.sealed_complete && !summary.claimed_total_bytes_verified);
        #[cfg(windows)]
        {
            assert!(OpenOptions::new().write(true).open(&fixture.path).is_err());
            assert!(fs::remove_file(&fixture.path).is_err());
        }
    }
}
