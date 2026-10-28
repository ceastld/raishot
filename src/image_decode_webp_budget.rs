//! 普通 WebP 解码前的可见工作区观察，不渲染主图像素。
//!
//! 辅助图必须解开熵流才能定位后续表；它们不带变换或递归熵图。
//! 主图只读变换及各组代码表，主像素熵流的完整性仍由像素解码器检查。
//! 这里的上界覆盖锁定后端的 Rust 缓冲，
//! 不是分配器或操作系统私有内存的硬限制。

use std::{
    mem::size_of,
    sync::atomic::{AtomicBool, Ordering},
};

use super::{MAX_DECODE_BYTES, MAX_DECODE_PIXELS};

type Result<T> = crate::Result<T>;

// 观察器的代码表、颜色缓存和读表临时数组均为固定大小栈对象。
// 此余量也包含返回错误时的小型文本和容器观察状态。
const OBSERVER_SCRATCH: u64 = 256 * 1024;
const CODEC_FIXED: u64 = 64 * 1024;
const VP8_FIXED: u64 = 18 * 1024 * 1024;
const MAX_ALPHABET: usize = 280 + (1 << 11);
const TREE_SHELL: u64 = 128;
const TREE_NODE: u64 = 32;
const LENGTH_SCRATCH: u64 = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WebpWorkspace {
    pub dimensions: (u32, u32),
    /// 公开 facade 的目标缓冲保守按 4P；无 alpha 的真实目标可能只有 3P。
    pub native_bytes: u64,
    /// 不包含调用者已有的来源与目标图。
    pub constructor_bytes: u64,
    /// 不包含调用者已有的来源与目标图；解码器退出后释放。
    pub decode_extra_bytes: u64,
    /// facade 不传递分配限制，因此不能拿 backend limit 代替此观察。
    pub backend_limit_floor: u64,
}

fn invalid_error() -> Box<dyn std::error::Error> {
    tr!("WebP数据或代码表无效", "Invalid WebP data or code table").into()
}

fn invalid<T>() -> Result<T> {
    Err(invalid_error())
}

fn budget<T>() -> Result<T> {
    Err(tr!(
        "WebP解码工作区超过可用预算",
        "WebP decode workspace exceeds the available budget"
    )
    .into())
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(tr!("图片读取已取消", "Image reading canceled").into())
    } else {
        Ok(())
    }
}

fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).ok_or_else(workspace_overflow)
}

fn mul(a: u64, b: u64) -> Result<u64> {
    a.checked_mul(b).ok_or_else(workspace_overflow)
}

fn workspace_overflow() -> Box<dyn std::error::Error> {
    tr!("WebP工作区容量溢出", "WebP workspace capacity overflow").into()
}

fn fits(external: u64, extra: u64) -> Result<()> {
    if add(external, extra)? > MAX_DECODE_BYTES {
        budget()
    } else {
        Ok(())
    }
}

fn pixels(dimensions: (u32, u32)) -> Result<u64> {
    let (w, h) = dimensions;
    let p = mul(u64::from(w), u64::from(h))?;
    if w == 0 || h == 0 || w > 65_535 || h > 65_535 || p > MAX_DECODE_PIXELS {
        return budget();
    }
    Ok(p)
}

fn le24(bytes: &[u8]) -> Result<u32> {
    let Some(b) = bytes.get(..3) else {
        return invalid();
    };
    Ok(u32::from(b[0]) | (u32::from(b[1]) << 8) | (u32::from(b[2]) << 16))
}

fn le32(bytes: &[u8]) -> Result<u32> {
    let Some(b) = bytes.get(..4) else {
        return invalid();
    };
    Ok(u32::from_le_bytes(b.try_into()?))
}

#[derive(Clone, Copy)]
struct Chunk<'a> {
    kind: &'a [u8],
    data: &'a [u8],
}

struct Chunks<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Chunks<'a> {
    fn next(&mut self, cancel: &AtomicBool) -> Result<Option<Chunk<'a>>> {
        check_cancel(cancel)?;
        if self.position == self.bytes.len() {
            return Ok(None);
        }
        let header_end = self.position.checked_add(8).ok_or_else(invalid_error)?;
        let Some(header) = self.bytes.get(self.position..header_end) else {
            return invalid();
        };
        let length = le32(&header[4..])? as usize;
        let end = header_end.checked_add(length).ok_or_else(invalid_error)?;
        let padded = end.checked_add(length & 1).ok_or_else(invalid_error)?;
        let Some(data) = self.bytes.get(header_end..end) else {
            return invalid();
        };
        if padded > self.bytes.len() {
            return invalid();
        }
        self.position = padded;
        Ok(Some(Chunk {
            kind: &header[..4],
            data,
        }))
    }
}

#[derive(Default)]
struct ImageChunks<'a> {
    vp8: Option<&'a [u8]>,
    lossless: Option<&'a [u8]>,
    alpha: Option<&'a [u8]>,
}

impl<'a> ImageChunks<'a> {
    fn insert(&mut self, chunk: Chunk<'a>) {
        match chunk.kind {
            b"VP8 " => {
                self.vp8.get_or_insert(chunk.data);
            }
            b"VP8L" => {
                self.lossless.get_or_insert(chunk.data);
            }
            b"ALPH" => {
                self.alpha.get_or_insert(chunk.data);
            }
            _ => {}
        }
    }
}

struct Container<'a> {
    dimensions: (u32, u32),
    frame_dimensions: (u32, u32),
    image: ImageChunks<'a>,
    animated: bool,
    alpha: bool,
}

fn container<'a>(bytes: &'a [u8], cancel: &AtomicBool) -> Result<Container<'a>> {
    if bytes.get(..4) != Some(b"RIFF")
        || bytes.get(8..12) != Some(b"WEBP")
        || u64::from(le32(bytes.get(4..).ok_or_else(invalid_error)?)?) + 8 != bytes.len() as u64
    {
        return invalid();
    }
    let mut chunks = Chunks {
        bytes: &bytes[12..],
        position: 0,
    };
    let Some(first) = chunks.next(cancel)? else {
        return invalid();
    };
    if first.kind == b"VP8 " || first.kind == b"VP8L" {
        let (dimensions, alpha) = if first.kind == b"VP8 " {
            (vp8_dimensions(first.data)?, false)
        } else {
            lossless_dimensions(first.data)?
        };
        // 后端只消费首个简单图块；仍逐块检查冻结输入的范围和取消。
        while chunks.next(cancel)?.is_some() {}
        let mut image = ImageChunks::default();
        image.insert(first);
        return Ok(Container {
            dimensions,
            frame_dimensions: dimensions,
            image,
            animated: false,
            alpha,
        });
    }
    if first.kind != b"VP8X" || first.data.len() != 10 {
        return invalid();
    }
    let flags = first.data[0];
    if flags & 0xc1 != 0 || first.data[1..4] != [0, 0, 0] {
        return invalid();
    }
    let dimensions = (le24(&first.data[4..])? + 1, le24(&first.data[7..])? + 1);
    pixels(dimensions)?;
    let animated = flags & 2 != 0;
    let mut image = ImageChunks::default();
    let mut first_frame = None;
    let mut anim = None;
    while let Some(chunk) = chunks.next(cancel)? {
        match chunk.kind {
            b"ANIM" => {
                anim.get_or_insert(chunk.data);
            }
            b"ANMF" => {
                if chunk.data.len() < 24 {
                    return invalid();
                }
                first_frame.get_or_insert(chunk.data);
            }
            _ => image.insert(chunk),
        }
    }
    let frame_dimensions;
    if animated {
        if anim.is_none_or(|a| a.len() != 6) {
            return invalid();
        }
        let Some(frame) = first_frame else {
            return invalid();
        };
        let x = u64::from(le24(frame)?) * 2;
        let y = u64::from(le24(&frame[3..])?) * 2;
        frame_dimensions = (le24(&frame[6..])? + 1, le24(&frame[9..])? + 1);
        if frame[15] & !3 != 0
            || add(x, u64::from(frame_dimensions.0))? > u64::from(dimensions.0)
            || add(y, u64::from(frame_dimensions.1))? > u64::from(dimensions.1)
        {
            return invalid();
        }
        image = ImageChunks::default();
        let mut sub = Chunks {
            bytes: &frame[16..],
            position: 0,
        };
        let Some(first) = sub.next(cancel)? else {
            return invalid();
        };
        image.insert(first);
        match first.kind {
            b"VP8 " | b"VP8L" => {}
            b"ALPH" => {
                // 后端在 ALPH 后直接把紧邻 payload 当 VP8 解码；必须绑定
                // 这一块，不能跳过未知块再从后方寻找另一个 VP8 头。
                let Some(next) = sub.next(cancel)? else {
                    return invalid();
                };
                if next.kind != b"VP8 " {
                    return invalid();
                }
                image.insert(next);
            }
            _ => return invalid(),
        }
        // read_image 只读第一帧的这一组图块；后续块仍核固定范围。
        while sub.next(cancel)?.is_some() {}
    } else {
        // 非动画容器不能携带 ANMF。后端仍会把其子块并入静态图目录，
        // 若只观察顶层 VP8，便可能漏掉实际优先读取的 VP8L。
        if first_frame.is_some() {
            return invalid();
        }
        frame_dimensions = dimensions;
    }
    if image.vp8.is_some() == image.lossless.is_some()
        || image.lossless.is_some() && image.alpha.is_some()
    {
        return invalid();
    }
    Ok(Container {
        dimensions,
        frame_dimensions,
        image,
        animated,
        alpha: flags & 0x10 != 0,
    })
}

fn vp8_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 10 || bytes[0] & 1 != 0 || bytes[3..6] != [0x9d, 1, 0x2a] {
        return invalid();
    }
    let dimensions = (
        u32::from(u16::from_le_bytes([bytes[6], bytes[7]]) & 0x3fff),
        u32::from(u16::from_le_bytes([bytes[8], bytes[9]]) & 0x3fff),
    );
    pixels(dimensions)?;
    if (le24(bytes)? >> 5) as usize > bytes.len() - 10 {
        return invalid();
    }
    Ok(dimensions)
}

fn lossless_dimensions(bytes: &[u8]) -> Result<((u32, u32), bool)> {
    if bytes.first() != Some(&0x2f) {
        return invalid();
    }
    let header = le32(bytes.get(1..).ok_or_else(invalid_error)?)?;
    if header >> 29 != 0 {
        return invalid();
    }
    let dimensions = ((header & 0x3fff) + 1, ((header >> 14) & 0x3fff) + 1);
    pixels(dimensions)?;
    Ok((dimensions, header & (1 << 28) != 0))
}

fn vp8_workspace(bytes: &[u8], dimensions: (u32, u32)) -> Result<(u64, u64)> {
    if vp8_dimensions(bytes)? != dimensions {
        return invalid();
    }
    let cols = u64::from(dimensions.0).div_ceil(16);
    let rows = u64::from(dimensions.1).div_ceil(16);
    let blocks = mul(cols, rows)?;
    let yuv = mul(mul(cols * 16, rows * 16)?, 3)? / 2;
    // 64B 覆盖一个宏块记录；三份覆盖 Vec 增长的新旧块同时存活。
    // 3C 覆盖分区、末分区 read_to_end 与复制；固定项覆盖一次坏的
    // 24bit 分区声明、首分区及概率表，不能只按成功读到的 C 计费。
    let extra = add(
        add(add(yuv, mul(blocks, 3 * 64)?)?, mul(cols, 64)?)?,
        add(
            add(
                mul(u64::from(dimensions.0), 2)?,
                mul(bytes.len() as u64, 3)?,
            )?,
            VP8_FIXED,
        )?,
    )?;
    Ok((extra, yuv))
}

/// `external_source_bytes` 已包括宿主驻留与冻结输入的实际 capacity。
/// 本观察器返回后不保留辅助图；调用者须分别在构造/解码阶段计入返回值。
pub(super) fn inspect(
    bytes: &[u8],
    external_source_bytes: u64,
    cancel: &AtomicBool,
) -> Result<WebpWorkspace> {
    inspect_with_limit(bytes, external_source_bytes, MAX_DECODE_BYTES, cancel)
}

/// 小于默认总额的来源也复用所有观察器分配门；虚拟占额只缩减可用量，
/// 不进入返回的codec工作区，也不改变调用方实际来源容量的校验。
pub(super) fn inspect_with_limit(
    bytes: &[u8],
    external_source_bytes: u64,
    total_limit: u64,
    cancel: &AtomicBool,
) -> Result<WebpWorkspace> {
    let result = (|| {
        check_cancel(cancel)?;
        if total_limit == 0
            || total_limit > MAX_DECODE_BYTES
            || external_source_bytes < bytes.len() as u64
        {
            return budget();
        }
        let reserved = add(external_source_bytes, MAX_DECODE_BYTES - total_limit)?;
        inspect_inner(bytes, reserved, cancel)
    })();
    check_cancel(cancel)?;
    result
}

fn inspect_inner(
    bytes: &[u8],
    external_source_bytes: u64,
    cancel: &AtomicBool,
) -> Result<WebpWorkspace> {
    check_cancel(cancel)?;
    if external_source_bytes < bytes.len() as u64 || bytes.len() as u64 > MAX_DECODE_BYTES {
        return budget();
    }
    fits(external_source_bytes, OBSERVER_SCRATCH)?;
    let info = container(bytes, cancel)?;
    let p = pixels(info.dimensions)?;
    let f = pixels(info.frame_dimensions)?;
    let native_bytes = mul(p, 4)?;
    let mut observer = Observer {
        external: external_source_bytes,
        cancel,
    };
    let private;
    if let Some(bytes) = info.image.lossless {
        if lossless_dimensions(bytes)?.0 != info.frame_dimensions {
            return invalid();
        }
        let model = observer.lossless(&bytes[5..], info.frame_dimensions)?;
        if info.animated {
            private = add(mul(f, 4)?, model.peak)?.max(add(mul(f, 4)?, mul(p, 4)?)?);
        } else {
            private = add(if info.alpha { 0 } else { mul(f, 4)? }, model.peak)?;
        }
    } else {
        let Some(bytes) = info.image.vp8 else {
            return invalid();
        };
        let (vp8, yuv) = vp8_workspace(bytes, info.frame_dimensions)?;
        let alpha = if let Some(alpha) = info.image.alpha {
            if info.alpha || info.animated {
                Some(observer.alpha(alpha, info.frame_dimensions)?)
            } else {
                None
            }
        } else {
            None
        };
        private = if info.animated {
            let decode = if let Some(alpha) = alpha {
                alpha.max(add(f, vp8)?).max(add(add(yuv, f)?, mul(f, 4)?)?)
            } else {
                vp8.max(add(yuv, mul(f, 3)?)?)
            };
            decode.max(add(mul(f, 4)?, mul(p, 4)?)?)
        } else if let Some(alpha) = alpha {
            vp8.max(add(yuv, alpha)?)
        } else {
            vp8
        };
    }
    let decode_extra_bytes = add(private, CODEC_FIXED)?;
    fits(external_source_bytes, CODEC_FIXED)?;
    fits(
        external_source_bytes,
        add(native_bytes, decode_extra_bytes)?,
    )?;
    check_cancel(cancel)?;
    Ok(WebpWorkspace {
        dimensions: info.dimensions,
        native_bytes,
        constructor_bytes: CODEC_FIXED,
        decode_extra_bytes,
        backend_limit_floor: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        output.extend_from_slice(kind);
        output.extend_from_slice(&(data.len() as u32).to_le_bytes());
        output.extend_from_slice(data);
        if data.len() & 1 != 0 {
            output.push(0);
        }
    }

    fn finish(mut output: Vec<u8>) -> Vec<u8> {
        let length = (output.len() - 8) as u32;
        output[4..8].copy_from_slice(&length.to_le_bytes());
        output
    }

    fn simple(payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
        chunk(&mut bytes, b"VP8L", payload);
        finish(bytes)
    }

    fn extended(dimensions: (u32, u32), flags: u8) -> Vec<u8> {
        let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
        let mut header = [flags, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        header[4..7].copy_from_slice(&(dimensions.0 - 1).to_le_bytes()[..3]);
        header[7..10].copy_from_slice(&(dimensions.1 - 1).to_le_bytes()[..3]);
        chunk(&mut bytes, b"VP8X", &header);
        bytes
    }

    #[derive(Default)]
    struct Writer {
        data: Vec<u8>,
        bit: usize,
    }

    impl Writer {
        fn put(&mut self, value: u32, count: u8) {
            for shift in 0..count {
                if self.bit / 8 == self.data.len() {
                    self.data.push(0);
                }
                self.data[self.bit / 8] |= (((value >> shift) & 1) as u8) << (self.bit & 7);
                self.bit += 1;
            }
        }

        fn code(&mut self, value: u32, count: u8) {
            for shift in (0..count).rev() {
                self.put((value >> shift) & 1, 1);
            }
        }

        fn single(&mut self, value: u8) {
            self.put(1, 1);
            self.put(0, 1);
            self.put(1, 1);
            self.put(u32::from(value), 8);
        }

        fn pair(&mut self, zero: u8, one: u8) {
            self.put(1, 1);
            self.put(1, 1);
            self.put(1, 1);
            self.put(u32::from(zero), 8);
            self.put(u32::from(one), 8);
        }

        fn normal(&mut self, alphabet: usize, nonzero: &[(usize, u8)]) {
            self.put(0, 1);
            self.put(1, 4); // 五个 code-length 描述，依次 17/18/0/1/2。
            for length in [0, 0, 1, 2, 2] {
                self.put(length, 3);
            }
            self.put(0, 1);
            for symbol in 0..alphabet {
                let length = nonzero
                    .iter()
                    .find(|&&(s, _)| s == symbol)
                    .map_or(0, |&(_, l)| l);
                match length {
                    0 => self.code(0, 1),
                    1 => self.code(2, 2),
                    2 => self.code(3, 2),
                    _ => panic!("fixture length"),
                }
            }
        }

        fn constant_auxiliary(&mut self, rgba: [u8; 4]) {
            self.put(0, 1); // 辅助图无 cache，也没有 meta 位或变换位。
            for value in [rgba[1], rgba[0], rgba[2], rgba[3], 0] {
                self.single(value);
            }
        }

        fn complex_auxiliary(&mut self) {
            // 四像素模式图：literal、cache、两像素 LZ。两种 normal 代码长度。
            let argb = 0xff00_0100u32; // RGBA=(0,1,0,255)
            let cache_index = (0x1e35a7bdu32.wrapping_mul(argb) >> 31) as usize;
            self.put(1, 1);
            self.put(1, 4);
            self.normal(282, &[(1, 1), (257, 2), (280 + cache_index, 2)]);
            for value in [0, 0, 255, 1] {
                self.single(value);
            }
            self.code(0, 1); // literal green=1
            self.code(3, 2); // cache
            self.code(2, 2); // length=2, plane=2 -> distance=1
        }
    }

    fn lossless_header(dimensions: (u32, u32), alpha: bool) -> Vec<u8> {
        let word = (dimensions.0 - 1) | ((dimensions.1 - 1) << 14) | (u32::from(alpha) << 28);
        let mut bytes = vec![0x2f];
        bytes.extend_from_slice(&word.to_le_bytes());
        bytes
    }

    fn complex_payload() -> Vec<u8> {
        let mut bits = Writer::default();
        // 17 色 palette 不缩宽，继而两幅 2×2 非常量编码模式图和 subtract-green。
        bits.put(1, 1);
        bits.put(3, 2);
        bits.put(16, 8);
        bits.constant_auxiliary([0; 4]);
        for kind in [0, 1] {
            bits.put(1, 1);
            bits.put(kind, 2);
            bits.put(0, 3);
            bits.complex_auxiliary();
        }
        bits.put(1, 1);
        bits.put(2, 2);
        bits.put(0, 1); // 变换终止
        bits.put(0, 1); // 主 cache
        bits.put(1, 1);
        bits.put(0, 3); // 2×2 meta map
        bits.put(0, 1); // meta map cache
        bits.normal(280, &[(0, 2), (1, 2), (2, 2), (3, 2)]);
        for value in [0, 0, 0, 0] {
            bits.single(value);
        }
        for value in 0..4 {
            bits.code(value, 2);
        }
        for _ in 0..4 {
            for _ in 0..5 {
                bits.single(0);
            }
        }
        let mut payload = lossless_header((8, 8), true);
        payload.extend_from_slice(&bits.data);
        payload.extend_from_slice(&[0; 8]);
        payload
    }

    #[test]
    fn image_decode_webp_budget_real_lossy_compressed_and_raw_alpha() {
        let bytes = include_bytes!("../tests/data/image_decode/static_vp8_alpha.webp");
        let observed = inspect(bytes, bytes.len() as u64, &token()).unwrap();
        assert_eq!(observed.dimensions, (64, 32));
        assert_eq!(observed.native_bytes, 64 * 32 * 4);
        assert!(observed.decode_extra_bytes >= VP8_FIXED);
        let original = image::load_from_memory_with_format(bytes, image::ImageFormat::WebP)
            .unwrap()
            .into_rgba8();
        let mut chunks = Chunks {
            bytes: &bytes[12..],
            position: 0,
        };
        let mut raw = extended((64, 32), 0x10);
        let mut alpha = vec![0];
        alpha.extend(original.pixels().map(|p| p[3]));
        chunk(&mut raw, b"ALPH", &alpha);
        while let Some(part) = chunks.next(&token()).unwrap() {
            if part.kind == b"VP8 " {
                chunk(&mut raw, b"VP8 ", part.data);
            }
        }
        let raw = finish(raw);
        assert_eq!(
            inspect(&raw, raw.len() as u64, &token())
                .unwrap()
                .dimensions,
            (64, 32)
        );
        let decoded = image::load_from_memory_with_format(&raw, image::ImageFormat::WebP)
            .unwrap()
            .into_rgba8();
        assert_eq!(decoded.as_raw(), original.as_raw());
    }

    #[test]
    fn image_decode_webp_budget_real_encoder_opaque_and_straight_alpha() {
        for alpha in [false, true] {
            let channels = if alpha { 4 } else { 3 };
            let mut pixels = Vec::new();
            for y in 0..8 {
                for x in 0..16 {
                    pixels.extend_from_slice(&[
                        (x * 17) as u8,
                        (y * 31) as u8,
                        ((x + y) * 9) as u8,
                    ]);
                    if alpha {
                        pixels.push((x * y * 7) as u8);
                    }
                }
            }
            assert_eq!(pixels.len(), 16 * 8 * channels);
            let mut bytes = Vec::new();
            image::codecs::webp::WebPEncoder::new_lossless(&mut bytes)
                .encode(
                    &pixels,
                    16,
                    8,
                    if alpha {
                        image::ExtendedColorType::Rgba8
                    } else {
                        image::ExtendedColorType::Rgb8
                    },
                )
                .unwrap();
            let workspace = inspect(&bytes, bytes.capacity() as u64, &token()).unwrap();
            assert_eq!(workspace.dimensions, (16, 8));
            assert_eq!(workspace.native_bytes, 16 * 8 * 4);
            if !alpha {
                assert!(workspace.decode_extra_bytes >= 16 * 8 * 4);
            }
            let decoded =
                image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP).unwrap();
            if alpha {
                assert_eq!(decoded.into_rgba8().into_raw(), pixels);
            } else {
                assert_eq!(decoded.into_rgb8().into_raw(), pixels);
            }
        }
    }

    #[test]
    fn image_decode_webp_budget_all_transforms_normal_cache_lz_and_meta_groups() {
        let bytes = simple(&complex_payload());
        let workspace = inspect(&bytes, bytes.len() as u64, &token()).unwrap();
        assert_eq!(workspace.dimensions, (8, 8));
        assert!(workspace.decode_extra_bytes > CODEC_FIXED + 4 * 5 * TREE_SHELL);
        let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP)
            .unwrap()
            .into_rgba8();
        assert_eq!(image.dimensions(), (8, 8));
        assert!(image.pixels().all(|p| p.0[..3] == [0, 0, 0]));
    }

    #[test]
    fn image_decode_webp_budget_normal_repeat_codes_and_shortened_description() {
        let mut bits = Writer::default();
        bits.put(0, 1);
        bits.put(0, 1);
        bits.put(0, 1);
        bits.put(0, 1); // 普通 green 树。
        bits.put(5, 4); // 九个 code-length 字段，包括 repeat16。
        for length in [3, 3, 2, 0, 2, 0, 0, 0, 2] {
            bits.put(length, 3);
        }
        bits.put(1, 1);
        bits.put(0, 3);
        bits.put(3, 2); // 五个编码 token。
        bits.code(1, 2); // 长度2。
        bits.code(2, 2);
        bits.put(0, 2); // repeat16: 再三项长度2。
        bits.code(6, 3);
        bits.put(7, 3); // repeat17: 十项0。
        bits.code(7, 3);
        bits.put(127, 7); // repeat18: 138项0。
        bits.code(7, 3);
        bits.put(117, 7); // repeat18: 128项0，共280项。
        for value in [0, 0, 255, 0] {
            bits.single(value);
        }
        for _ in 0..4 {
            bits.code(0, 2);
        }
        let mut payload = lossless_header((2, 2), true);
        payload.extend_from_slice(&bits.data);
        payload.extend_from_slice(&[0; 8]);
        let bytes = simple(&payload);
        assert_eq!(
            inspect(&bytes, bytes.len() as u64, &token())
                .unwrap()
                .dimensions,
            (2, 2)
        );
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP)
            .unwrap()
            .into_rgba8();
        assert!(decoded.pixels().all(|p| p.0 == [0, 0, 0, 255]));
    }

    #[test]
    fn image_decode_webp_budget_observes_main_tables_without_decoding_main_pixels() {
        let mut bits = Writer::default();
        bits.put(0, 1);
        bits.put(0, 1);
        bits.put(0, 1); // transform/cache/meta
        bits.pair(0, 1);
        for value in [0, 0, 255, 0] {
            bits.single(value);
        }
        let mut payload = lossless_header((128, 128), true);
        payload.extend_from_slice(&bits.data);
        let truncated = simple(&payload);
        let workspace = inspect(&truncated, truncated.len() as u64, &token()).unwrap();
        assert!(image::load_from_memory_with_format(&truncated, image::ImageFormat::WebP).is_err());
        payload.resize(payload.len() + 128 * 128 / 8 + 8, 0);
        let complete = simple(&payload);
        assert_eq!(
            inspect(&complete, complete.len() as u64, &token()).unwrap(),
            workspace
        );
        assert!(image::load_from_memory_with_format(&complete, image::ImageFormat::WebP).is_ok());
    }

    #[test]
    fn image_decode_webp_budget_first_animation_frame_includes_local_and_canvas() {
        let payload = complex_payload();
        let mut bytes = extended((16, 8), 0x12);
        chunk(&mut bytes, b"ANIM", &[0; 6]);
        let mut frame = vec![0; 16];
        frame[6] = 7;
        frame[9] = 7;
        chunk(&mut frame, b"VP8L", &payload);
        chunk(&mut bytes, b"ANMF", &frame);
        let bytes = finish(bytes);
        let workspace = inspect(&bytes, bytes.len() as u64, &token()).unwrap();
        assert_eq!(workspace.dimensions, (16, 8));
        assert!(workspace.decode_extra_bytes >= CODEC_FIXED + 16 * 8 * 4 + 8 * 8 * 4);
        let decoded =
            image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (16, 8));
    }

    #[test]
    fn image_decode_webp_budget_binds_animation_flag_and_immediately_adjacent_codec() {
        let original = include_bytes!("../tests/data/image_decode/static_vp8_alpha.webp");
        let mut chunks = Chunks {
            bytes: &original[12..],
            position: 0,
        };
        let mut vp8 = None;
        while let Some(part) = chunks.next(&token()).unwrap() {
            if part.kind == b"VP8 " {
                vp8 = Some(part.data);
            }
        }
        let vp8 = vp8.unwrap();
        let mut lossless = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut lossless)
            .encode(
                &vec![0; 64 * 32 * 4],
                64,
                32,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        let mut lp_chunks = Chunks {
            bytes: &lossless[12..],
            position: 0,
        };
        let lp = lp_chunks.next(&token()).unwrap().unwrap();
        assert_eq!(lp.kind, b"VP8L");
        let mut frame_header = vec![0; 16];
        frame_header[6] = 63;
        frame_header[9] = 31;

        let mut static_bytes = extended((64, 32), 0);
        chunk(&mut static_bytes, b"VP8 ", vp8);
        let mut hidden_lossless = frame_header.clone();
        chunk(&mut hidden_lossless, b"VP8L", lp.data);
        chunk(&mut static_bytes, b"ANMF", &hidden_lossless);
        let static_bytes = finish(static_bytes);
        // flag=0 的 ANMF 不能向静态后端字典塞入另一个优先级更高的 codec。
        assert!(inspect(&static_bytes, static_bytes.len() as u64, &token()).is_err());

        let mut animated = extended((64, 32), 0x12);
        chunk(&mut animated, b"ANIM", &[0; 6]);
        let mut frame = frame_header;
        chunk(&mut frame, b"ALPH", &vec![0; 64 * 32 + 1]);
        let mut disguised = vp8.to_vec();
        disguised[6..8].copy_from_slice(&128u16.to_le_bytes());
        disguised[8..10].copy_from_slice(&64u16.to_le_bytes());
        chunk(&mut frame, b"JUNK", &disguised);
        chunk(&mut frame, b"VP8 ", vp8);
        chunk(&mut animated, b"ANMF", &frame);
        let animated = finish(animated);
        // 不能跳过 ALPH 后的块，转而拿后方的小 VP8 为真实解码器作预算。
        assert!(inspect(&animated, animated.len() as u64, &token()).is_err());
    }

    #[test]
    fn image_decode_webp_budget_exact_resident_limit_dimensions_and_cancellation() {
        let bytes = include_bytes!("../tests/data/image_decode/static_vp8_alpha.webp");
        let workspace = inspect(bytes, bytes.len() as u64, &token()).unwrap();
        let external = MAX_DECODE_BYTES - workspace.native_bytes - workspace.decode_extra_bytes;
        assert_eq!(inspect(bytes, external, &token()).unwrap(), workspace);
        assert!(inspect(bytes, external + 1, &token()).is_err());
        assert!(inspect(bytes, bytes.len() as u64 - 1, &token()).is_err());
        let mut mismatch = bytes.to_vec();
        assert_eq!(&mismatch[12..16], b"VP8X");
        mismatch[24] = 31; // 容器宽 32，实际 VP8 宽 64。
        assert!(inspect(&mismatch, mismatch.len() as u64, &token()).is_err());
        let error = inspect(bytes, bytes.len() as u64, &AtomicBool::new(true))
            .unwrap_err()
            .to_string();
        assert!(error.contains("取消") || error.contains("canceled"));
    }

    #[test]
    fn image_webp_frame_budget_observer_respects_lower_limit_before_auxiliary_allocation() {
        let bytes = simple(&complex_payload());
        let resident = bytes.capacity() as u64;
        let workspace = inspect(&bytes, resident, &token()).unwrap();
        let phase = workspace
            .constructor_bytes
            .max(workspace.native_bytes + workspace.decode_extra_bytes)
            .max(OBSERVER_SCRATCH + workspace.native_bytes.max(1024));
        assert_eq!(
            inspect_with_limit(&bytes, resident, resident + phase, &token()).unwrap(),
            workspace
        );
        // 允许固定观察对象，却容不下首幅2×2辅助图时，必须在reserve前拒绝。
        assert!(
            inspect_with_limit(&bytes, resident, resident + OBSERVER_SCRATCH + 15, &token())
                .is_err()
        );
        assert!(inspect_with_limit(&bytes, 0, MAX_DECODE_BYTES / 2, &token()).is_err());
        assert!(inspect_with_limit(&bytes, resident, 0, &token()).is_err());
        assert!(inspect_with_limit(&bytes, resident, MAX_DECODE_BYTES + 1, &token()).is_err());
        let error = inspect_with_limit(&bytes, 0, 0, &AtomicBool::new(true))
            .unwrap_err()
            .to_string();
        assert!(error.contains("取消") || error.contains("canceled"));
    }

    #[test]
    fn image_decode_webp_budget_rejects_bad_auxiliary_backreference_and_huffman_tree() {
        let cancel = token();
        let mut observer = Observer {
            external: 0,
            cancel: &cancel,
        };
        let mut bits = Writer::default();
        bits.put(0, 1);
        bits.normal(280, &[(257, 1), (258, 1)]);
        for value in [0, 0, 0, 1] {
            bits.single(value);
        }
        bits.put(0, 1); // 首像素就是反向引用，不得读取未初始化像素。
        assert!(observer
            .auxiliary(
                &mut Bits {
                    bytes: &bits.data,
                    bit: 0
                },
                (2, 2)
            )
            .is_err());
        assert!(Tree::lengths(&[2, 2]).is_err());
        assert!(Tree::lengths(&[1, 1, 1]).is_err());
        assert!(Tree::lengths(&[0; 19]).is_err());
        // 辅助图上限先于 reserve，不需要分配大图即可验证。
        observer.external = MAX_DECODE_BYTES - OBSERVER_SCRATCH;
        assert!(observer
            .auxiliary(&mut Bits { bytes: &[], bit: 0 }, (1, 1))
            .is_err());
    }

    #[test]
    fn image_decode_webp_budget_auxiliary_cache_keeps_unused_zero_slot_semantics() {
        let expected = [[0, 1, 0, 255], [0; 4], [0; 4], [0, 1, 0, 255]];
        assert_eq!(0x1e35a7bdu32.wrapping_mul(0xff00_0100) >> 31, 0);
        let cancel = token();
        for main in [false, true] {
            let mut bits = Writer::default();
            if main {
                bits.put(0, 1);
            } // 主图变换终止。
            bits.put(1, 1);
            bits.put(1, 4); // 两个 cache 槽。
            if main {
                bits.put(0, 1);
            } // 主图无 meta。
            bits.normal(282, &[(1, 2), (256, 2), (280, 2), (281, 2)]);
            for value in [0, 0, 255, 1] {
                bits.single(value);
            }
            for code in [0, 3, 1, 2] {
                bits.code(code, 2);
            }
            bits.data.extend_from_slice(&[0; 8]);
            if main {
                let mut payload = lossless_header((2, 2), true);
                payload.extend_from_slice(&bits.data);
                let bytes = simple(&payload);
                inspect(&bytes, bytes.len() as u64, &cancel).unwrap();
                let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP)
                    .unwrap()
                    .into_rgba8()
                    .into_raw();
                assert_eq!(decoded, expected.into_iter().flatten().collect::<Vec<_>>());
            } else {
                let observer = Observer {
                    external: 0,
                    cancel: &cancel,
                };
                let (decoded, _) = observer
                    .auxiliary(
                        &mut Bits {
                            bytes: &bits.data,
                            bit: 0,
                        },
                        (2, 2),
                    )
                    .unwrap();
                assert_eq!(decoded, expected);
            }
        }
    }
}

struct Bits<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> Bits<'a> {
    fn read(&mut self, count: u8) -> Result<u32> {
        if count > 24 {
            return invalid();
        }
        let end = self
            .bit
            .checked_add(usize::from(count))
            .ok_or_else(invalid_error)?;
        if end > self.bytes.len().saturating_mul(8) {
            return invalid();
        }
        let mut value = 0;
        for shift in 0..count {
            value |= u32::from((self.bytes[self.bit / 8] >> (self.bit & 7)) & 1) << shift;
            self.bit += 1;
        }
        Ok(value)
    }
}

// 每次只保留一组辅助图代码表。主图逐表复用此固定空间，不保存其全部组。
struct Tree {
    single: Option<u16>,
    pair: Option<[u16; 2]>,
    counts: [u16; 16],
    first: [u32; 16],
    offsets: [u16; 16],
    symbols: [u16; MAX_ALPHABET],
    max_length: u8,
    native_heap: u64,
}

impl Tree {
    fn empty() -> Self {
        Self {
            single: None,
            pair: None,
            counts: [0; 16],
            first: [0; 16],
            offsets: [0; 16],
            symbols: [0; MAX_ALPHABET],
            max_length: 0,
            native_heap: 0,
        }
    }

    fn single(symbol: u16) -> Self {
        let mut tree = Self::empty();
        tree.single = Some(symbol);
        tree
    }

    fn lengths(lengths: &[u16]) -> Result<Self> {
        if lengths.len() > MAX_ALPHABET {
            return invalid();
        }
        let mut tree = Self::empty();
        let mut nonzero = 0;
        for &length in lengths {
            if length > 15 {
                return invalid();
            }
            if length != 0 {
                tree.counts[usize::from(length)] += 1;
                nonzero += 1;
                tree.max_length = tree.max_length.max(length as u8);
            }
        }
        if nonzero == 0 {
            return invalid();
        }
        if nonzero == 1 {
            return Ok(Self::single(
                lengths
                    .iter()
                    .position(|&n| n != 0)
                    .ok_or_else(invalid_error)? as u16,
            ));
        }
        let mut code = 0u32;
        let mut offset = 0u16;
        for length in 1..=usize::from(tree.max_length) {
            tree.first[length] = code;
            tree.offsets[length] = offset;
            let count = u32::from(tree.counts[length]);
            if code + count > 1 << length {
                return invalid();
            }
            code = (code + count) << 1;
            offset += tree.counts[length];
        }
        if code != 2 << tree.max_length {
            return invalid();
        }
        let mut positions = tree.offsets;
        for (symbol, &length) in lengths
            .iter()
            .enumerate()
            .filter(|(_, length)| **length != 0)
        {
            let position = &mut positions[usize::from(length)];
            tree.symbols[usize::from(*position)] = symbol as u16;
            *position += 1;
        }
        let table_bits = tree.max_length.min(10);
        let long = tree.counts[usize::from(table_bits) + 1..]
            .iter()
            .map(|&n| u64::from(n))
            .sum::<u64>();
        // 完整二叉子树最多 2L 个节点。32B 保守覆盖锁版节点；表项为 u32。
        tree.native_heap = add(4u64 << table_bits, mul(long, 2 * TREE_NODE)?)?;
        Ok(tree)
    }

    fn symbol(&self, bits: &mut Bits<'_>) -> Result<u16> {
        if let Some(symbol) = self.single {
            return Ok(symbol);
        }
        if let Some(pair) = self.pair {
            return Ok(pair[bits.read(1)? as usize]);
        }
        let mut code = 0u32;
        for length in 1..=usize::from(self.max_length) {
            code = (code << 1) | bits.read(1)?;
            if let Some(index) = code.checked_sub(self.first[length]) {
                if index < u32::from(self.counts[length]) {
                    return Ok(self.symbols[usize::from(self.offsets[length]) + index as usize]);
                }
            }
        }
        invalid()
    }
}

const LENGTH_ORDER: [usize; 19] = [
    17, 18, 0, 1, 2, 3, 4, 5, 16, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
];

fn read_tree(bits: &mut Bits<'_>, alphabet: usize) -> Result<Tree> {
    if alphabet < 2 || alphabet > MAX_ALPHABET {
        return invalid();
    }
    if bits.read(1)? != 0 {
        let two = bits.read(1)? != 0;
        let width = 1 + 7 * bits.read(1)? as u8;
        let zero = bits.read(width)? as u16;
        if usize::from(zero) >= alphabet {
            return invalid();
        }
        if !two {
            return Ok(Tree::single(zero));
        }
        let one = bits.read(8)? as u16;
        if usize::from(one) >= alphabet {
            return invalid();
        }
        let mut tree = Tree::empty();
        tree.pair = Some([zero, one]);
        tree.native_heap = 3 * TREE_NODE + 2 * 4;
        return Ok(tree);
    }
    let mut description = [0u16; 19];
    let count = 4 + bits.read(4)? as usize;
    for &index in &LENGTH_ORDER[..count] {
        description[index] = bits.read(3)? as u16;
    }
    let code_lengths = Tree::lengths(&description)?;
    let mut maximum = if bits.read(1)? != 0 {
        let width = 2 + 2 * bits.read(3)? as u8;
        let maximum = 2 + bits.read(width)? as usize;
        if maximum > alphabet {
            return invalid();
        }
        maximum
    } else {
        alphabet
    };
    let mut lengths = [0u16; MAX_ALPHABET];
    let mut position = 0;
    let mut previous = 8;
    while position < alphabet && maximum != 0 {
        maximum -= 1;
        let value = code_lengths.symbol(bits)?;
        if value < 16 {
            lengths[position] = value;
            position += 1;
            if value != 0 {
                previous = value;
            }
        } else {
            let (width, offset, length) = match value {
                16 => (2, 3, previous),
                17 => (3, 3, 0),
                18 => (7, 11, 0),
                _ => return invalid(),
            };
            let repeat = bits.read(width)? as usize + offset;
            let end = position.checked_add(repeat).ok_or_else(invalid_error)?;
            if end > alphabet {
                return invalid();
            }
            lengths[position..end].fill(length);
            position = end;
        }
    }
    Tree::lengths(&lengths[..alphabet])
}

fn cache_bits(bits: &mut Bits<'_>) -> Result<u8> {
    if bits.read(1)? == 0 {
        return Ok(0);
    }
    let count = bits.read(4)? as u8;
    if !(1..=11).contains(&count) {
        return invalid();
    }
    Ok(count)
}

fn group(bits: &mut Bits<'_>, cache: u8) -> Result<[Tree; 5]> {
    Ok([
        read_tree(bits, 280 + if cache == 0 { 0 } else { 1 << cache })?,
        read_tree(bits, 256)?,
        read_tree(bits, 256)?,
        read_tree(bits, 256)?,
        read_tree(bits, 40)?,
    ])
}

fn prefix(bits: &mut Bits<'_>, code: u16) -> Result<usize> {
    if code < 4 {
        return Ok(usize::from(code + 1));
    }
    let extra = ((code - 2) / 2) as u8;
    if extra > 18 {
        return invalid();
    }
    Ok(((2 + (usize::from(code) & 1)) << extra) + bits.read(extra)? as usize + 1)
}

#[rustfmt::skip]
const DISTANCES: [(i8, i8); 120] = [
    (0,1),(1,0),(1,1),(-1,1),(0,2),(2,0),(1,2),(-1,2),
    (2,1),(-2,1),(2,2),(-2,2),(0,3),(3,0),(1,3),(-1,3),
    (3,1),(-3,1),(2,3),(-2,3),(3,2),(-3,2),(0,4),(4,0),
    (1,4),(-1,4),(4,1),(-4,1),(3,3),(-3,3),(2,4),(-2,4),
    (4,2),(-4,2),(0,5),(3,4),(-3,4),(4,3),(-4,3),(5,0),
    (1,5),(-1,5),(5,1),(-5,1),(2,5),(-2,5),(5,2),(-5,2),
    (4,4),(-4,4),(3,5),(-3,5),(5,3),(-5,3),(0,6),(6,0),
    (1,6),(-1,6),(6,1),(-6,1),(2,6),(-2,6),(6,2),(-6,2),
    (4,5),(-4,5),(5,4),(-5,4),(3,6),(-3,6),(6,3),(-6,3),
    (0,7),(7,0),(1,7),(-1,7),(5,5),(-5,5),(7,1),(-7,1),
    (4,6),(-4,6),(6,4),(-6,4),(2,7),(-2,7),(7,2),(-7,2),
    (3,7),(-3,7),(7,3),(-7,3),(5,6),(-5,6),(6,5),(-6,5),
    (8,0),(4,7),(-4,7),(7,4),(-7,4),(8,1),(8,2),(6,6),
    (-6,6),(8,3),(5,7),(-5,7),(7,5),(-7,5),(8,4),(6,7),
    (-6,7),(7,6),(-7,6),(8,5),(7,7),(-7,7),(8,6),(8,7),
];

fn distance(width: u32, code: usize) -> Result<usize> {
    if code > 120 {
        return Ok(code - 120);
    }
    if code == 0 {
        return invalid();
    }
    let (x, y) = DISTANCES[code - 1];
    Ok((i64::from(x) + i64::from(y) * i64::from(width)).max(1) as usize)
}

fn cache_insert(cache: &mut [[u8; 4]; 2048], bits: u8, rgba: [u8; 4]) {
    if bits == 0 {
        return;
    }
    let [r, g, b, a] = rgba;
    let argb = (u32::from(a) << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b);
    let index = (0x1e35a7bdu32.wrapping_mul(argb)) >> (32 - bits);
    cache[index as usize] = rgba;
}

#[derive(Clone, Copy, Default)]
struct LosslessModel {
    peak: u64,
    maps: u64,
}

struct Observer<'a> {
    external: u64,
    cancel: &'a AtomicBool,
}

impl Observer<'_> {
    fn auxiliary(
        &self,
        bits: &mut Bits<'_>,
        dimensions: (u32, u32),
    ) -> Result<(Vec<[u8; 4]>, u64)> {
        check_cancel(self.cancel)?;
        let count = usize::try_from(pixels(dimensions)?)?;
        let bytes = mul(count as u64, size_of::<[u8; 4]>() as u64)?;
        fits(self.external, add(OBSERVER_SCRATCH, bytes)?)?;
        let mut output = Vec::new();
        output.try_reserve_exact(count)?;
        fits(
            self.external,
            add(OBSERVER_SCRATCH, mul(output.capacity() as u64, 4)?)?,
        )?;
        output.resize(count, [0; 4]);
        let cache = cache_bits(bits)?;
        let trees = group(bits, cache)?;
        let heaps = trees
            .iter()
            .try_fold(0, |n, tree| add(n, tree.native_heap))?;
        let native = add(
            bytes,
            add(
                heaps,
                add(
                    if cache == 0 { 0 } else { 4 << cache },
                    LENGTH_SCRATCH + 4 * 5 * TREE_SHELL,
                )?,
            )?,
        )?;
        let mut colors = [[0u8; 4]; 2048];
        let mut position = 0;
        while position < count {
            if position & 1023 == 0 {
                check_cancel(self.cancel)?;
            }
            let green = trees[0].symbol(bits)?;
            if green < 256 {
                let pixel = [
                    trees[1].symbol(bits)? as u8,
                    green as u8,
                    trees[2].symbol(bits)? as u8,
                    trees[3].symbol(bits)? as u8,
                ];
                output[position] = pixel;
                cache_insert(&mut colors, cache, pixel);
                position += 1;
            } else if green < 280 {
                let length = prefix(bits, green - 256)?;
                let distance_code = trees[4].symbol(bits)?;
                let plane = prefix(bits, distance_code)?;
                let distance = distance(dimensions.0, plane)?;
                if distance > position || length > count - position {
                    return invalid();
                }
                for _ in 0..length {
                    if position & 1023 == 0 {
                        check_cancel(self.cancel)?;
                    }
                    let pixel = output[position - distance];
                    output[position] = pixel;
                    // 连续同值快路径不更新 cache；来自未用透明槽的值也
                    // 不得因此覆盖另一个槽，否则后续 meta 图可能被误读。
                    if distance != 1 {
                        cache_insert(&mut colors, cache, pixel);
                    }
                    position += 1;
                }
            } else {
                let index = usize::from(green - 280);
                if cache == 0 || index >= 1 << cache {
                    return invalid();
                }
                output[position] = colors[index];
                position += 1;
            }
        }
        check_cancel(self.cancel)?;
        Ok((output, native))
    }

    fn lossless(&mut self, bytes: &[u8], dimensions: (u32, u32)) -> Result<LosslessModel> {
        let mut bits = Bits { bytes, bit: 0 };
        let mut width = dimensions.0;
        let mut seen = 0u8;
        let mut model = LosslessModel::default();
        while bits.read(1)? != 0 {
            check_cancel(self.cancel)?;
            let kind = bits.read(2)? as u8;
            if seen & (1 << kind) != 0 {
                return invalid();
            }
            seen |= 1 << kind;
            let map_dimensions = match kind {
                0 | 1 => {
                    let shift = bits.read(3)? + 2;
                    Some((
                        width.div_ceil(1 << shift),
                        dimensions.1.div_ceil(1 << shift),
                    ))
                }
                2 => None,
                3 => Some((bits.read(8)? + 1, 1)),
                _ => return invalid(),
            };
            if let Some(map_dimensions) = map_dimensions {
                let (map, work) = self.auxiliary(&mut bits, map_dimensions)?;
                model.peak = model.peak.max(add(model.maps, work)?);
                model.maps = add(model.maps, mul(map.len() as u64, 4)?)?;
                // 原解码器保留变换图；观察器只需经过其码流，不重复持有全部图。
                drop(map);
                if kind == 3 {
                    let shift = if map_dimensions.0 <= 2 {
                        3
                    } else if map_dimensions.0 <= 4 {
                        2
                    } else if map_dimensions.0 <= 16 {
                        1
                    } else {
                        0
                    };
                    width = width.div_ceil(1 << shift);
                }
            }
        }
        let cache = cache_bits(&mut bits)?;
        let cache_bytes = if cache == 0 { 0 } else { 4 << cache };
        let base = add(model.maps, cache_bytes)?;
        let mut groups = 1u64;
        let mut entropy_bytes = 0;
        if bits.read(1)? != 0 {
            let shift = bits.read(3)? + 2;
            let map_dimensions = (
                width.div_ceil(1 << shift),
                dimensions.1.div_ceil(1 << shift),
            );
            let (map, work) = self.auxiliary(&mut bits, map_dimensions)?;
            model.peak = model.peak.max(add(base, work)?);
            for (index, pixel) in map.iter().enumerate() {
                if index & 1023 == 0 {
                    check_cancel(self.cancel)?;
                }
                groups = groups.max(u64::from(u16::from(pixel[0]) << 8 | u16::from(pixel[1])) + 1);
            }
            entropy_bytes = mul(map.len() as u64, 2)?;
            // 转换 Vec<u16> 时原 RGBA 辅助图尚未释放。
            model.peak = model.peak.max(add(base, mul(map.len() as u64, 6)?)?);
            drop(map);
        }
        let mut heaps = 0;
        let table_base = add(base, entropy_bytes)?;
        // 组 Vec 从空增长；三倍容量覆盖旧块与新块，至少首个四组容量。
        let shells = mul(groups.max(4), 3 * 5 * TREE_SHELL)?;
        for _ in 0..groups {
            check_cancel(self.cancel)?;
            for alphabet in [
                280 + if cache == 0 { 0 } else { 1 << cache },
                256,
                256,
                256,
                40,
            ] {
                let tree = read_tree(&mut bits, alphabet)?;
                heaps = add(heaps, tree.native_heap)?;
                let peak = add(table_base, add(heaps, add(shells, LENGTH_SCRATCH)?)?)?;
                // 在进一步扫描巨大组目录之前也先验证已确定的后端工作集。
                fits(self.external, peak)?;
                model.peak = model.peak.max(peak);
            }
        }
        // 调色板的展开查找表/一行索引和小型变换目录，不额外扩整图。
        model.peak = model
            .peak
            .max(add(model.maps, add(8192, u64::from(dimensions.0))?)?);
        check_cancel(self.cancel)?;
        Ok(model)
    }

    fn alpha(&mut self, bytes: &[u8], dimensions: (u32, u32)) -> Result<u64> {
        let Some(&header) = bytes.first() else {
            return invalid();
        };
        if header & 0xc0 != 0 || (header >> 4) & 3 > 1 {
            return invalid();
        }
        let p = pixels(dimensions)?;
        match header & 3 {
            0 => {
                if bytes.len() as u64 != p + 1 {
                    return invalid();
                }
                Ok(p)
            }
            1 => {
                let model = self.lossless(&bytes[1..], dimensions)?;
                // 熵表在解码像素后释放；提取 green 时 RGBA、green 与变换图同时在场。
                Ok(add(mul(p, 4)?, model.peak)?.max(add(mul(p, 5)?, model.maps)?))
            }
            _ => invalid(),
        }
    }
}
