//! 录屏库的有界 MP4 容器探测；不初始化编码器，也不读取媒体样本。
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VideoInfo {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration: Option<Duration>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    Complete(VideoInfo),
    Unavailable,
    Cancelled,
}

const MAX_BOXES: usize = 256;
const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_INTERRUPTED_READS: usize = 8;

/// Complete 证明所检查的容器边界完整；它不保证样本可解码。
/// 零尺寸开放结尾盒、截断和超预算返回 Unavailable，真实 I/O 错误原样返回。
pub(crate) fn probe(path: &Path, cancelled: &dyn Fn() -> bool) -> io::Result<Probe> {
    if cancelled() {
        return Ok(Probe::Cancelled);
    }
    let mut file = open_read_only(path)?;
    let before = file.metadata()?;
    if !is_regular_file(&before) {
        return Ok(Probe::Unavailable);
    }
    let result = probe_reader(&mut file, before.len(), cancelled, Limits::default())?;
    if matches!(result, Probe::Complete(_)) {
        let after = file.metadata()?;
        if cancelled() {
            return Ok(Probe::Cancelled);
        }
        if !is_regular_file(&after)
            || before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
        {
            return Ok(Probe::Unavailable);
        }
    }
    Ok(result)
}

#[cfg(windows)]
fn open_read_only(path: &Path) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_FLAG_OPEN_REPARSE_POINT：路径检查与打开之间被替换的末级链接不能被跟随。
    OpenOptions::new()
        .read(true)
        .custom_flags(0x0020_0000)
        .open(path)
}

#[cfg(not(windows))]
fn open_read_only(path: &Path) -> io::Result<File> {
    File::open(path)
}

fn is_regular_file(metadata: &Metadata) -> bool {
    if !metadata.is_file() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT：按同一句柄的实际属性拒绝重解析文件。
        metadata.file_attributes() & 0x0000_0400 == 0
    }
    #[cfg(not(windows))]
    {
        true
    }
}

#[derive(Clone, Copy)]
struct Limits {
    boxes: usize,
    bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            boxes: MAX_BOXES,
            bytes: MAX_READ_BYTES,
        }
    }
}

enum Rejection {
    Unavailable,
    Cancelled,
    Io(io::Error),
}

type ParseResult<T> = Result<T, Rejection>;

struct Reader<'a, R> {
    input: &'a mut R,
    length: u64,
    cancelled: &'a dyn Fn() -> bool,
    limits: Limits,
    boxes: usize,
    bytes: usize,
}

#[derive(Clone, Copy)]
struct Atom {
    kind: [u8; 4],
    payload: u64,
    end: u64,
}

impl<R: Read + Seek> Reader<'_, R> {
    fn check_cancel(&self) -> ParseResult<()> {
        if (self.cancelled)() {
            Err(Rejection::Cancelled)
        } else {
            Ok(())
        }
    }

    fn read_at(&mut self, offset: u64, output: &mut [u8]) -> ParseResult<()> {
        self.check_cancel()?;
        let end = offset
            .checked_add(output.len() as u64)
            .ok_or(Rejection::Unavailable)?;
        let bytes = self
            .bytes
            .checked_add(output.len())
            .ok_or(Rejection::Unavailable)?;
        if end > self.length || bytes > self.limits.bytes {
            return Err(Rejection::Unavailable);
        }
        self.bytes = bytes;
        self.input
            .seek(SeekFrom::Start(offset))
            .map_err(Rejection::Io)?;
        self.check_cancel()?;
        // 每个字段至多 96 bytes；短读、Interrupted 重试间也检查取消。
        // 不使用无限重试 Interrupted 的 read_exact，真实持续 I/O 错误原样返回。
        let mut filled = 0;
        let mut interrupted = 0;
        while filled < output.len() {
            self.check_cancel()?;
            match self.input.read(&mut output[filled..]) {
                Ok(0) => return Err(Rejection::Unavailable),
                Ok(count) => filled += count,
                Err(error)
                    if error.kind() == io::ErrorKind::Interrupted
                        && interrupted < MAX_INTERRUPTED_READS =>
                {
                    interrupted += 1;
                }
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                    return Err(Rejection::Unavailable);
                }
                Err(error) => return Err(Rejection::Io(error)),
            }
        }
        self.check_cancel()
    }

    fn atom(&mut self, offset: u64, parent_end: u64) -> ParseResult<Atom> {
        self.check_cancel()?;
        if self.boxes >= self.limits.boxes || parent_end.saturating_sub(offset) < 8 {
            return Err(Rejection::Unavailable);
        }
        self.boxes += 1;
        let mut header = [0; 8];
        self.read_at(offset, &mut header)?;
        let size32 = u32::from_be_bytes(header[..4].try_into().unwrap());
        let kind = header[4..].try_into().unwrap();
        let (size, mut header_size) = match size32 {
            // 开放结尾没有可核验的最终边界，保守排除仍在写入的内容。
            0 => return Err(Rejection::Unavailable),
            1 => {
                if parent_end.saturating_sub(offset) < 16 {
                    return Err(Rejection::Unavailable);
                }
                let mut extended = [0; 8];
                self.read_at(offset + 8, &mut extended)?;
                (u64::from_be_bytes(extended), 16)
            }
            size => (u64::from(size), 8),
        };
        if kind == *b"uuid" {
            header_size += 16;
        }
        if size < header_size {
            return Err(Rejection::Unavailable);
        }
        let end = offset.checked_add(size).ok_or(Rejection::Unavailable)?;
        if end > parent_end || end > self.length {
            return Err(Rejection::Unavailable);
        }
        Ok(Atom {
            kind,
            payload: offset + header_size,
            end,
        })
    }

    fn prefix<const N: usize>(&mut self, atom: Atom) -> ParseResult<([u8; N], usize)> {
        let count = (atom.end - atom.payload).min(N as u64) as usize;
        let mut bytes = [0; N];
        self.read_at(atom.payload, &mut bytes[..count])?;
        Ok((bytes, count))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Root,
    Movie,
    Track,
    Media,
    OtherContainer,
}

#[derive(Clone, Copy, Default)]
struct Track {
    dimensions: Option<(u32, u32)>,
    seen_header: bool,
    is_video: bool,
    seen_handler: bool,
}

#[derive(Clone, Copy)]
struct Scope {
    kind: ScopeKind,
    end: u64,
    track: Track,
}

impl Scope {
    fn new(kind: ScopeKind, end: u64) -> Self {
        Self {
            kind,
            end,
            track: Track::default(),
        }
    }
}

fn probe_reader<R: Read + Seek>(
    input: &mut R,
    length: u64,
    cancelled: &dyn Fn() -> bool,
    limits: Limits,
) -> io::Result<Probe> {
    let mut reader = Reader {
        input,
        length,
        cancelled,
        limits,
        boxes: 0,
        bytes: 0,
    };
    match parse(&mut reader) {
        Ok(info) => Ok(Probe::Complete(info)),
        Err(Rejection::Unavailable) => Ok(Probe::Unavailable),
        Err(Rejection::Cancelled) => Ok(Probe::Cancelled),
        Err(Rejection::Io(error)) => Err(error),
    }
}

fn parse<R: Read + Seek>(reader: &mut Reader<'_, R>) -> ParseResult<VideoInfo> {
    let mut stack = [Scope::new(ScopeKind::Root, 0); MAX_DEPTH];
    stack[0].end = reader.length;
    let mut depth = 1;
    let mut position = 0;
    let mut info = VideoInfo::default();
    let (mut file_type, mut movie, mut media_data, mut seen_duration) =
        (false, false, false, false);
    loop {
        reader.check_cancel()?;
        let scope = stack[depth - 1];
        if position == scope.end {
            if let (ScopeKind::Track, true, None, Some((width, height))) = (
                scope.kind,
                scope.track.is_video,
                info.width,
                scope.track.dimensions,
            ) {
                info.width = Some(width);
                info.height = Some(height);
            }
            depth -= 1;
            if depth == 0 {
                break;
            }
            continue;
        }
        let atom = reader.atom(position, scope.end)?;
        position = atom.end;
        let next_scope = match (scope.kind, atom.kind) {
            (ScopeKind::Root, kind) if kind == *b"ftyp" => {
                let payload_size = atom.end - atom.payload;
                if file_type || payload_size < 8 || !payload_size.is_multiple_of(4) {
                    return Err(Rejection::Unavailable);
                }
                let mut brand = [0; 8];
                reader.read_at(atom.payload, &mut brand)?;
                file_type = true;
                None
            }
            (ScopeKind::Root, kind) if kind == *b"mdat" => {
                if atom.end == atom.payload {
                    return Err(Rejection::Unavailable);
                }
                media_data = true;
                None
            }
            (ScopeKind::Root, kind) if kind == *b"moov" => {
                if movie {
                    return Err(Rejection::Unavailable);
                }
                movie = true;
                Some(ScopeKind::Movie)
            }
            (ScopeKind::Movie, kind) if kind == *b"mvhd" => {
                let (bytes, count) = reader.prefix::<32>(atom)?;
                // 多个 mvhd 不能选一个猜时长；容器仍完整，时长未知。
                info.duration = if seen_duration {
                    None
                } else {
                    movie_duration(&bytes[..count])
                };
                seen_duration = true;
                None
            }
            (ScopeKind::Movie, kind) if kind == *b"trak" => Some(ScopeKind::Track),
            (ScopeKind::Track, kind) if kind == *b"tkhd" => {
                let (bytes, count) = reader.prefix::<96>(atom)?;
                let track = &mut stack[depth - 1].track;
                track.dimensions = if track.seen_header {
                    None
                } else {
                    track_dimensions(&bytes[..count])
                };
                track.seen_header = true;
                None
            }
            (ScopeKind::Track, kind) if kind == *b"mdia" => Some(ScopeKind::Media),
            (ScopeKind::Media, kind) if kind == *b"hdlr" => {
                let (bytes, count) = reader.prefix::<12>(atom)?;
                if let Some(track_scope) = stack[..depth]
                    .iter_mut()
                    .rev()
                    .find(|scope| scope.kind == ScopeKind::Track)
                {
                    let track = &mut track_scope.track;
                    track.is_video = !track.seen_handler
                        && count >= 12
                        && bytes[0] == 0
                        && bytes[8..12] == *b"vide";
                    track.seen_handler = true;
                }
                None
            }
            (kind, name)
                if kind != ScopeKind::Root
                    && matches!(&name, b"minf" | b"stbl" | b"edts" | b"dinf" | b"mvex") =>
            {
                Some(ScopeKind::OtherContainer)
            }
            _ => None,
        };
        if let Some(kind) = next_scope {
            if depth == MAX_DEPTH {
                return Err(Rejection::Unavailable);
            }
            stack[depth] = Scope::new(kind, atom.end);
            depth += 1;
            position = atom.payload;
        }
    }
    if !file_type || !movie || !media_data {
        return Err(Rejection::Unavailable);
    }
    Ok(info)
}

fn movie_duration(bytes: &[u8]) -> Option<Duration> {
    let (scale, ticks) = match bytes.first()? {
        0 if bytes.len() >= 20 => (
            u32::from_be_bytes(bytes[12..16].try_into().ok()?),
            u64::from(u32::from_be_bytes(bytes[16..20].try_into().ok()?)),
        ),
        1 if bytes.len() >= 32 => (
            u32::from_be_bytes(bytes[20..24].try_into().ok()?),
            u64::from_be_bytes(bytes[24..32].try_into().ok()?),
        ),
        _ => return None,
    };
    // 全 1 是未知时长标记，不能显示为极长影片。
    if scale == 0 || (bytes[0] == 0 && ticks == u64::from(u32::MAX)) || ticks == u64::MAX {
        return None;
    }
    let scale = u64::from(scale);
    let nanos = (ticks % scale) * 1_000_000_000 / scale;
    Some(Duration::new(ticks / scale, nanos as u32))
}

fn track_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let (matrix_start, dimension_start) = match bytes.first()? {
        0 if bytes.len() >= 84 => (40, 76),
        1 if bytes.len() >= 96 => (52, 88),
        _ => return None,
    };
    let width = u32::from_be_bytes(
        bytes[dimension_start..dimension_start + 4]
            .try_into()
            .ok()?,
    );
    let height = u32::from_be_bytes(
        bytes[dimension_start + 4..dimension_start + 8]
            .try_into()
            .ok()?,
    );
    if width == 0 || height == 0 || width & 0xffff != 0 || height & 0xffff != 0 {
        return None;
    }
    let mut matrix = [0_i32; 9];
    for (entry, value) in matrix
        .iter_mut()
        .zip(bytes[matrix_start..matrix_start + 36].chunks_exact(4))
    {
        *entry = i32::from_be_bytes(value.try_into().ok()?);
    }
    if matrix[2] != 0 || matrix[5] != 0 || matrix[8] != 0x4000_0000 {
        return None;
    }
    // 只接受无缩放的四个正交旋转。平移不会改变视频尺寸。
    let same_axes = matches!(
        [matrix[0], matrix[1], matrix[3], matrix[4]],
        [65_536, 0, 0, 65_536] | [-65_536, 0, 0, -65_536]
    );
    let swapped_axes = matches!(
        [matrix[0], matrix[1], matrix[3], matrix[4]],
        [0, 65_536, -65_536, 0] | [0, -65_536, 65_536, 0]
    );
    if same_axes {
        Some((width >> 16, height >> 16))
    } else if swapped_axes {
        Some((height >> 16, width >> 16))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::io::Cursor;

    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn extended(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = 1_u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(&((payload.len() + 16) as u64).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn ftyp() -> Vec<u8> {
        atom(b"ftyp", b"isom\0\0\0\0")
    }

    fn movie(duration: Vec<u8>, tracks: Vec<u8>) -> Vec<u8> {
        atom(b"moov", &[duration, tracks].concat())
    }

    fn mvhd(version: u8, scale: u32, ticks: u64) -> Vec<u8> {
        let mut payload = vec![0; if version == 1 { 32 } else { 20 }];
        payload[0] = version;
        if version == 1 {
            payload[20..24].copy_from_slice(&scale.to_be_bytes());
            payload[24..32].copy_from_slice(&ticks.to_be_bytes());
        } else {
            payload[12..16].copy_from_slice(&scale.to_be_bytes());
            payload[16..20].copy_from_slice(&(ticks as u32).to_be_bytes());
        }
        atom(b"mvhd", &payload)
    }

    fn tkhd(version: u8, width: u32, height: u32, rotation: u16) -> Vec<u8> {
        let (length, matrix, dimensions) = if version == 1 {
            (96, 52, 88)
        } else {
            (84, 40, 76)
        };
        let mut payload = vec![0; length];
        payload[0] = version;
        let axes = match rotation {
            90 => [0, 65_536, -65_536, 0],
            180 => [-65_536, 0, 0, -65_536],
            270 => [0, -65_536, 65_536, 0],
            _ => [65_536, 0, 0, 65_536],
        };
        let transform: [i32; 9] = [axes[0], axes[1], 0, axes[2], axes[3], 0, 0, 0, 0x4000_0000];
        for (i, value) in transform.into_iter().enumerate() {
            payload[matrix + i * 4..matrix + i * 4 + 4].copy_from_slice(&value.to_be_bytes());
        }
        payload[dimensions..dimensions + 4].copy_from_slice(&(width << 16).to_be_bytes());
        payload[dimensions + 4..dimensions + 8].copy_from_slice(&(height << 16).to_be_bytes());
        atom(b"tkhd", &payload)
    }

    fn track(header: Vec<u8>, handler: &[u8; 4]) -> Vec<u8> {
        let mut payload = vec![0; 12];
        payload[8..12].copy_from_slice(handler);
        atom(
            b"trak",
            &[header, atom(b"mdia", &atom(b"hdlr", &payload))].concat(),
        )
    }

    fn completed(moov: Vec<u8>) -> Vec<u8> {
        [ftyp(), atom(b"mdat", &[1, 2, 3, 4]), moov].concat()
    }

    fn memory(bytes: &[u8]) -> Probe {
        probe_reader(
            &mut Cursor::new(bytes),
            bytes.len() as u64,
            &|| false,
            Limits::default(),
        )
        .unwrap()
    }

    #[test]
    fn 尾部电影盒读取视频尺寸与精确时长() {
        let bytes = completed(movie(
            mvhd(0, 1_000, 2_345),
            track(tkhd(0, 1920, 1080, 0), b"vide"),
        ));
        assert_eq!(
            memory(&bytes),
            Probe::Complete(VideoInfo {
                width: Some(1920),
                height: Some(1080),
                duration: Some(Duration::from_millis(2_345)),
            })
        );
    }

    #[test]
    fn 扩展长度盒和版本一电影轨道可解析() {
        let moov = movie(
            mvhd(1, 90_000, 9_000_045_000),
            track(tkhd(1, 803, 537, 0), b"vide"),
        );
        let bytes = [
            extended(b"ftyp", b"isom\0\0\0\0"),
            extended(b"mdat", &[1]),
            moov,
        ]
        .concat();
        assert_eq!(
            memory(&bytes),
            Probe::Complete(VideoInfo {
                width: Some(803),
                height: Some(537),
                duration: Some(Duration::from_millis(100_000_500)),
            })
        );
    }

    #[test]
    fn 正交旋转转换显示尺寸() {
        for (rotation, dimensions) in [
            (0, (1920, 1080)),
            (90, (1080, 1920)),
            (180, (1920, 1080)),
            (270, (1080, 1920)),
        ] {
            let bytes = completed(movie(
                Vec::new(),
                track(tkhd(0, 1920, 1080, rotation), b"vide"),
            ));
            let Probe::Complete(info) = memory(&bytes) else {
                panic!("完整容器未通过");
            };
            assert_eq!(
                (info.width, info.height),
                (Some(dimensions.0), Some(dimensions.1))
            );
        }
    }

    #[test]
    fn 未知非正交矩阵不猜尺寸() {
        let mut header = tkhd(0, 1920, 1080, 0);
        header[48..52].copy_from_slice(&32_768_i32.to_be_bytes());
        let bytes = completed(movie(Vec::new(), track(header, b"vide")));
        assert_eq!(memory(&bytes), Probe::Complete(VideoInfo::default()));
    }

    #[test]
    fn 音频轨道尺寸不会冒充视频尺寸() {
        let tracks = [
            track(tkhd(0, 1, 1, 0), b"soun"),
            track(tkhd(0, 640, 480, 0), b"vide"),
        ]
        .concat();
        let Probe::Complete(info) = memory(&completed(movie(Vec::new(), tracks))) else {
            panic!("完整容器未通过");
        };
        assert_eq!((info.width, info.height), (Some(640), Some(480)));
    }

    #[test]
    fn 无元数据的完整容器使用未知字段() {
        assert_eq!(
            memory(&completed(movie(Vec::new(), Vec::new()))),
            Probe::Complete(VideoInfo::default())
        );
    }

    #[test]
    fn 无电影盒或媒体内容拒绝() {
        assert_eq!(
            memory(&[ftyp(), atom(b"mdat", &[1])].concat()),
            Probe::Unavailable
        );
        assert_eq!(
            memory(&[ftyp(), movie(Vec::new(), Vec::new())].concat()),
            Probe::Unavailable
        );
        assert_eq!(
            memory(&[ftyp(), atom(b"mdat", &[]), movie(Vec::new(), Vec::new())].concat()),
            Probe::Unavailable
        );
    }

    #[test]
    fn 任意尾部截断不伪装完整() {
        let bytes = completed(movie(mvhd(0, 30, 90), track(tkhd(0, 640, 480, 0), b"vide")));
        for length in 0..bytes.len() {
            assert_eq!(
                memory(&bytes[..length]),
                Probe::Unavailable,
                "截断位置 {length}"
            );
        }
    }

    #[test]
    fn 开放结尾与过短扩展长度被拒绝() {
        let bytes = completed(movie(Vec::new(), Vec::new()));
        let mut zero = bytes.clone();
        zero[16..20].fill(0);
        assert_eq!(memory(&zero), Probe::Unavailable);
        for length in [0_u64, 8, 15, u64::MAX] {
            let mut bad = 1_u32.to_be_bytes().to_vec();
            bad.extend_from_slice(b"mdat");
            bad.extend_from_slice(&length.to_be_bytes());
            assert_eq!(memory(&bad), Probe::Unavailable);
        }
    }

    #[test]
    fn 子盒越出父边界或碎尾被拒绝() {
        let mut oversized = atom(b"free", &[]);
        oversized[..4].copy_from_slice(&9_u32.to_be_bytes());
        assert_eq!(
            memory(&completed(atom(b"moov", &oversized))),
            Probe::Unavailable
        );
        assert_eq!(
            memory(&completed(atom(b"moov", &[1, 2, 3]))),
            Probe::Unavailable
        );
    }

    #[test]
    fn 重复电影盒或文件类型被拒绝() {
        let bytes = completed(movie(Vec::new(), Vec::new()));
        assert_eq!(
            memory(&[bytes.clone(), movie(Vec::new(), Vec::new())].concat()),
            Probe::Unavailable
        );
        assert_eq!(memory(&[bytes, ftyp()].concat()), Probe::Unavailable);
    }

    #[test]
    fn 缺少或过短文件类型盒拒绝() {
        let rest = [atom(b"mdat", &[1]), movie(Vec::new(), Vec::new())].concat();
        assert_eq!(memory(&rest), Probe::Unavailable);
        assert_eq!(
            memory(&[atom(b"ftyp", b"isom"), rest].concat()),
            Probe::Unavailable
        );
    }

    #[test]
    fn 零时间基和未知时长标记不造时长() {
        for (version, scale, ticks) in [
            (0, 0, 30),
            (1, 0, 30),
            (0, 30, u64::from(u32::MAX)),
            (1, 30, u64::MAX),
        ] {
            let bytes = completed(movie(mvhd(version, scale, ticks), Vec::new()));
            assert_eq!(memory(&bytes), Probe::Complete(VideoInfo::default()));
        }
    }

    #[test]
    fn 时长余数只向下舍入不溢出() {
        let bytes = completed(movie(mvhd(1, u32::MAX, u64::MAX - 1), Vec::new()));
        let Probe::Complete(info) = memory(&bytes) else {
            panic!("完整容器未通过");
        };
        assert_eq!(
            info.duration,
            Some(Duration::new(
                (u64::MAX - 1) / u64::from(u32::MAX),
                999_999_999
            ))
        );
    }

    #[test]
    fn 超过盒数预算被拒绝() {
        let mut children = Vec::new();
        for _ in 0..MAX_BOXES {
            children.extend(atom(b"free", &[]));
        }
        assert_eq!(
            memory(&completed(atom(b"moov", &children))),
            Probe::Unavailable
        );
    }

    #[test]
    fn 超过字节预算被拒绝() {
        let bytes = completed(movie(Vec::new(), Vec::new()));
        let result = probe_reader(
            &mut Cursor::new(&bytes),
            bytes.len() as u64,
            &|| false,
            Limits {
                boxes: MAX_BOXES,
                bytes: 8,
            },
        )
        .unwrap();
        assert_eq!(result, Probe::Unavailable);
    }

    #[test]
    fn 恶意容器深度被固定栈拒绝() {
        let mut children = atom(b"free", &[]);
        for _ in 0..MAX_DEPTH {
            children = atom(b"minf", &children);
        }
        assert_eq!(
            memory(&completed(atom(b"moov", &children))),
            Probe::Unavailable
        );
    }

    #[test]
    fn 开始取消不读取输入() {
        let bytes = completed(movie(Vec::new(), Vec::new()));
        let mut cursor = Cursor::new(&bytes);
        assert_eq!(
            probe_reader(&mut cursor, bytes.len() as u64, &|| true, Limits::default()).unwrap(),
            Probe::Cancelled
        );
        assert_eq!(cursor.position(), 0);
    }

    #[test]
    fn 进行中取消返回独立结果() {
        let bytes = completed(movie(Vec::new(), Vec::new()));
        let count = Cell::new(0);
        let cancelled = || {
            count.set(count.get() + 1);
            count.get() >= 10
        };
        assert_eq!(
            probe_reader(
                &mut Cursor::new(&bytes),
                bytes.len() as u64,
                &cancelled,
                Limits::default()
            )
            .unwrap(),
            Probe::Cancelled
        );
    }

    struct CountingReader {
        cursor: Cursor<Vec<u8>>,
        bytes_read: usize,
        forbidden: std::ops::Range<u64>,
    }

    impl Read for CountingReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let start = self.cursor.position();
            assert!(!self.forbidden.contains(&start), "不能读取媒体 payload");
            assert!(
                start + output.len() as u64 <= self.forbidden.start || start >= self.forbidden.end,
                "不能跨入媒体 payload"
            );
            let count = self.cursor.read(output)?;
            self.bytes_read += count;
            Ok(count)
        }
    }

    impl Seek for CountingReader {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.cursor.seek(position)
        }
    }

    #[test]
    fn 大媒体盒仅跳过不读取() {
        let payload = vec![0x7f; 1024 * 1024];
        let bytes = [
            ftyp(),
            atom(b"mdat", &payload),
            movie(mvhd(0, 1, 2), Vec::new()),
        ]
        .concat();
        let length = bytes.len() as u64;
        let mut reader = CountingReader {
            cursor: Cursor::new(bytes),
            bytes_read: 0,
            forbidden: 24..24 + payload.len() as u64,
        };
        let result = probe_reader(&mut reader, length, &|| false, Limits::default()).unwrap();
        assert!(matches!(result, Probe::Complete(_)));
        assert!(reader.bytes_read < 128, "读取量不应随媒体体积增长");
    }

    struct ErrorReader;

    impl Read for ErrorReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "原始读取错误",
            ))
        }
    }

    impl Seek for ErrorReader {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    #[test]
    fn 读取错误保持原始种类与消息() {
        let error = probe_reader(&mut ErrorReader, 32, &|| false, Limits::default()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "原始读取错误");
    }

    #[test]
    fn 读取期间变短的文件不能通过完整资格() {
        let bytes = completed(movie(Vec::new(), Vec::new()));
        assert_eq!(
            probe_reader(
                &mut Cursor::new(&bytes[..20]),
                bytes.len() as u64,
                &|| false,
                Limits::default()
            )
            .unwrap(),
            Probe::Unavailable
        );
    }

    #[test]
    fn 用户类型盒必须容纳完整额外头() {
        let bytes = completed(movie(Vec::new(), atom(b"uuid", &[])));
        assert_eq!(memory(&bytes), Probe::Unavailable);
        let bytes = completed(movie(Vec::new(), atom(b"uuid", &[0; 16])));
        assert_eq!(memory(&bytes), Probe::Complete(VideoInfo::default()));
    }

    #[test]
    fn 重复时长和轨道头不选择有利值() {
        let duration = [mvhd(0, 1, 5), mvhd(0, 1, 7)].concat();
        let header = [tkhd(0, 640, 480, 0), tkhd(0, 1920, 1080, 0)].concat();
        let bytes = completed(movie(duration, track(header, b"vide")));
        assert_eq!(memory(&bytes), Probe::Complete(VideoInfo::default()));
    }

    struct InterruptedReader {
        reads: usize,
    }

    impl Read for InterruptedReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            Err(io::Error::new(io::ErrorKind::Interrupted, "持续中断"))
        }
    }

    impl Seek for InterruptedReader {
        fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
            Ok(0)
        }
    }

    #[test]
    fn 持续中断重试有界并保留错误() {
        let mut reader = InterruptedReader { reads: 0 };
        let error = probe_reader(&mut reader, 32, &|| false, Limits::default()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(error.to_string(), "持续中断");
        assert_eq!(reader.reads, MAX_INTERRUPTED_READS + 1);
    }

    #[test]
    fn 中断重试途中仍接受取消() {
        let mut reader = InterruptedReader { reads: 0 };
        let checks = Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            checks.get() >= 7
        };
        assert_eq!(
            probe_reader(&mut reader, 32, &cancelled, Limits::default()).unwrap(),
            Probe::Cancelled
        );
        assert!(reader.reads <= 2);
    }

    #[test]
    fn 非整数尺寸和透视矩阵不猜尺寸() {
        let mut fractional = tkhd(0, 1920, 1080, 0);
        fractional[84..88].copy_from_slice(&((1920_u32 << 16) + 1).to_be_bytes());
        let mut perspective = tkhd(0, 1920, 1080, 0);
        perspective[56..60].copy_from_slice(&1_i32.to_be_bytes());
        for header in [fractional, perspective] {
            let bytes = completed(movie(Vec::new(), track(header, b"vide")));
            assert_eq!(memory(&bytes), Probe::Complete(VideoInfo::default()));
        }
    }

    #[test]
    fn 短元数据盒不会越界但容器仍可完整() {
        let mut hdlr = vec![0; 12];
        hdlr[8..12].copy_from_slice(b"vide");
        let short_track = atom(
            b"trak",
            &[atom(b"tkhd", &[0]), atom(b"mdia", &atom(b"hdlr", &hdlr))].concat(),
        );
        let bytes = completed(movie(atom(b"mvhd", &[1]), short_track));
        assert_eq!(memory(&bytes), Probe::Complete(VideoInfo::default()));
    }
}
