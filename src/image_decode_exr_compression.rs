//! 串行EXR块的压缩工作区观察。仅借用来源，不构造解压块或整图。
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Result;

const MAX_BYTES: u64 = 256 * 1024 * 1024;
// 固定DEFLATE表、观察器32KiB历史、DWA线性查表及小型控制对象。
const FIXED: u64 = 256 * 1024;
// inflate预读扩容：stored LEN<=65535，另4KiB增长、296B fast-loop及16B fast-copy。
const INFLATE_SLACK: u64 = 65535 + 4096 + 296 + 16;
// PIZ: 65537个u64编码项、16384个<=64B解码项、long-list旧新增长，bitmap/LUT。
const HUFFMAN_WORK: u64 = 65537 * (8 + 12) + 16384 * 64 + 65536 * 2 + 8192;

#[derive(Clone, Copy, Debug)]
pub(super) struct Geometry {
    pub width: u32,
    pub height: u32,
    pub origin_y: i32,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Channel<'a> {
    pub name: &'a [u8],
    /// 0=U32，1=F16，2=F32，与容器声明相同。
    pub sample_type: u8,
    pub sampling_x: u32,
    pub sampling_y: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Workspace {
    /// 含后端的owned压缩块、返回raw块及中间分配；不含完整源或最终RGBA图。
    pub temporary_bytes: u64,
    /// ZIP/RLE共用的唯一TLS池，本块调用需要的长度；其它压缩为0。
    pub interleave_scratch_bytes: u64,
}

/// 同一短寿命解码线程上按实际读取顺序逐块累计；TLS必须在线程退出后清理。
/// 旧池与后续所有压缩块同驻，增长时另计旧、新池短时共存。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SerialWorkspace {
    pub peak_bytes: u64,
    pub scratch_bytes: u64,
}

impl SerialWorkspace {
    pub fn observe(&mut self, block: Workspace) -> Result<()> {
        let replacement = if block.interleave_scratch_bytes > self.scratch_bytes {
            block.interleave_scratch_bytes
        } else {
            0
        };
        let peak = add(&[block.temporary_bytes, self.scratch_bytes, replacement])?;
        self.peak_bytes = self.peak_bytes.max(peak);
        self.scratch_bytes = self.scratch_bytes.max(block.interleave_scratch_bytes);
        Ok(())
    }
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr!("图片读取已取消", "Image reading canceled").into())
    } else {
        Ok(())
    }
}
fn invalid() -> Box<dyn std::error::Error> {
    tr!(
        "EXR压缩块结构无效或被截断",
        "Invalid or truncated EXR compressed block"
    )
    .into()
}
fn budget() -> Box<dyn std::error::Error> {
    tr!(
        "EXR压缩工作区超过预算",
        "EXR compression workspace exceeds its budget"
    )
    .into()
}
fn unsupported() -> Box<dyn std::error::Error> {
    tr!(
        "EXR压缩器尚无工作区模型",
        "EXR compressor has no workspace model"
    )
    .into()
}
fn add(values: &[u64]) -> Result<u64> {
    values
        .iter()
        .try_fold(0u64, |sum, value| sum.checked_add(*value))
        .filter(|sum| *sum <= MAX_BYTES)
        .ok_or_else(budget)
}
fn mul(a: u64, b: u64) -> Result<u64> {
    a.checked_mul(b)
        .filter(|value| *value <= MAX_BYTES)
        .ok_or_else(budget)
}
fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    ))
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?,
    ))
}
fn inflate_peak(hint: u64) -> Result<u64> {
    mul(add(&[hint, INFLATE_SLACK])?, 3)
}
fn inflate_capacity(hint: u64) -> Result<u64> {
    mul(add(&[hint, INFLATE_SLACK])?, 2)
}
fn growth_peak(initial: u64, final_length: u64) -> Result<u64> {
    mul(initial.max(final_length).max(8), 3)
}
fn retained_capacity(initial: u64, final_length: u64) -> Result<u64> {
    mul(initial.max(final_length).max(8), 2)
}

struct Bits<'a> {
    bytes: &'a [u8],
    position: usize,
    cancel: &'a AtomicBool,
}
impl Bits<'_> {
    fn read(&mut self, count: usize) -> Result<u32> {
        let mut value = 0;
        for index in 0..count {
            if self.position & 32767 == 0 {
                cancelled(self.cancel)?;
            }
            let byte = *self.bytes.get(self.position / 8).ok_or_else(invalid)?;
            value |= u32::from((byte >> (self.position & 7)) & 1) << index;
            self.position += 1;
        }
        Ok(value)
    }
}

struct Tree {
    count: [u32; 16],
    first: [u32; 16],
    offset: [usize; 16],
    symbols: [u16; 288],
}
impl Tree {
    fn new(lengths: &[u8]) -> Result<Self> {
        let mut tree = Self {
            count: [0; 16],
            first: [0; 16],
            offset: [0; 16],
            symbols: [0; 288],
        };
        if lengths.len() > 288 {
            return Err(invalid());
        }
        for &length in lengths {
            if length > 15 {
                return Err(invalid());
            }
            if length != 0 {
                tree.count[length as usize] += 1;
            }
        }
        let (mut code, mut offset) = (0u32, 0usize);
        for length in 1..16 {
            code = (code + tree.count[length - 1]) << 1;
            if code + tree.count[length] > 1 << length {
                return Err(invalid());
            }
            tree.first[length] = code;
            tree.offset[length] = offset;
            offset += tree.count[length] as usize;
        }
        let mut next = tree.offset;
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                tree.symbols[next[length as usize]] = symbol as u16;
                next[length as usize] += 1;
            }
        }
        Ok(tree)
    }
    fn symbol(&self, bits: &mut Bits<'_>) -> Result<u16> {
        let mut code = 0;
        for length in 1..16 {
            code = (code << 1) | bits.read(1)?;
            if code >= self.first[length] && code - self.first[length] < self.count[length] {
                return Ok(self.symbols[self.offset[length] + (code - self.first[length]) as usize]);
            }
        }
        Err(invalid())
    }
}

fn trees(bits: &mut Bits<'_>, kind: u32) -> Result<(Tree, Tree)> {
    let mut lengths = [0u8; 320];
    if kind == 1 {
        lengths[..144].fill(8);
        lengths[144..256].fill(9);
        lengths[256..280].fill(7);
        lengths[280..288].fill(8);
        lengths[288..].fill(5);
        return Ok((Tree::new(&lengths[..288])?, Tree::new(&lengths[288..])?));
    }
    let literals = bits.read(5)? as usize + 257;
    let distances = bits.read(5)? as usize + 1;
    let precodes = bits.read(4)? as usize + 4;
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut code_lengths = [0u8; 19];
    for &index in &ORDER[..precodes] {
        code_lengths[index] = bits.read(3)? as u8;
    }
    let precode = Tree::new(&code_lengths)?;
    let mut position = 0;
    while position < literals + distances {
        let code = precode.symbol(bits)?;
        let (length, count) = match code {
            0..=15 => (code as u8, 1),
            16 if position != 0 => (lengths[position - 1], bits.read(2)? as usize + 3),
            17 => (0, bits.read(3)? as usize + 3),
            18 => (0, bits.read(7)? as usize + 11),
            _ => return Err(invalid()),
        };
        let end = position + count;
        if end > literals + distances {
            return Err(invalid());
        }
        lengths[position..end].fill(length);
        position = end;
    }
    if lengths[256] == 0 {
        return Err(invalid());
    }
    Ok((
        Tree::new(&lengths[..literals])?,
        Tree::new(&lengths[literals..literals + distances])?,
    ))
}

#[derive(Clone, Copy)]
struct RleCount {
    expected: u64,
    produced: u64,
    remaining: usize,
    pending: usize,
    literal: bool,
    stopped: bool,
}
impl RleCount {
    fn new(expected: u64) -> Self {
        Self {
            expected,
            produced: 0,
            remaining: 0,
            pending: 0,
            literal: false,
            stopped: false,
        }
    }
    fn byte(&mut self, byte: u8) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        if self.remaining != 0 {
            self.remaining -= 1;
            if self.remaining == 0 {
                self.produced = add(&[self.produced, self.pending as u64])?;
            }
            return Ok(());
        }
        if self.produced == self.expected {
            self.stopped = true;
            return Ok(());
        }
        let count = byte as i8 as i32;
        self.literal = count < 0;
        self.pending = if self.literal {
            (-count) as usize
        } else {
            count as usize + 1
        };
        self.remaining = if self.literal { self.pending } else { 1 };
        Ok(())
    }
    fn finish(self) -> Result<u64> {
        if self.remaining == 0 {
            Ok(self.produced)
        } else {
            Err(invalid())
        }
    }
}

/// DEFLATE只计结构输出；仅DWA RLE的token观察需要固定32KiB历史，不产生raw块。
struct InflateCount {
    count: u64,
    limit: u64,
    ring: [u8; 32768],
    rle: Option<RleCount>,
}
impl InflateCount {
    fn literal(&mut self, byte: u8) -> Result<()> {
        if self.count >= self.limit {
            return Err(invalid());
        }
        if let Some(rle) = &mut self.rle {
            self.ring[(self.count & 32767) as usize] = byte;
            rle.byte(byte)?;
        }
        self.count += 1;
        Ok(())
    }
    fn copy(&mut self, length: u64, distance: u64, cancel: &AtomicBool) -> Result<()> {
        if distance == 0 || distance > self.count || distance > 32768 {
            return Err(invalid());
        }
        let end = add(&[self.count, length])?;
        if end > self.limit {
            return Err(invalid());
        }
        if self.rle.is_none() {
            self.count = end;
            return Ok(());
        }
        for _ in 0..length {
            if self.count & 4095 == 0 {
                cancelled(cancel)?;
            }
            let byte = self.ring[((self.count - distance) & 32767) as usize];
            self.literal(byte)?;
        }
        Ok(())
    }
}

fn inflate(
    data: &[u8],
    maximum: u64,
    exact: bool,
    rle_goal: Option<u64>,
    cancel: &AtomicBool,
) -> Result<(u64, Option<u64>)> {
    cancelled(cancel)?;
    if maximum > MAX_BYTES || data.len() < 6 {
        return Err(invalid());
    }
    let flags = u16::from_be_bytes([data[0], data[1]]);
    if data[0] & 15 != 8 || data[0] >> 4 > 7 || flags % 31 != 0 {
        return Err(invalid());
    }
    // 后端从偏移2开始，不使用FDICT声明；这里不另跳过4字节以改变其实际范围。
    let mut bits = Bits {
        bytes: &data[2..],
        position: 0,
        cancel,
    };
    let mut out = InflateCount {
        count: 0,
        limit: maximum,
        ring: [0; 32768],
        rle: rle_goal.map(RleCount::new),
    };
    const LENGTH: [u16; 29] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
        131, 163, 195, 227, 258,
    ];
    const EXTRA: [u8; 29] = [
        0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
    ];
    const DISTANCE: [u16; 30] = [
        1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
        2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
    ];
    const DIST_EXTRA: [u8; 30] = [
        0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
        13, 13,
    ];
    loop {
        cancelled(cancel)?;
        let last = bits.read(1)?;
        let kind = bits.read(2)?;
        if kind == 0 {
            bits.position = (bits.position + 7) & !7;
            let length = bits.read(16)?;
            if bits.read(16)? != (!length & 65535) {
                return Err(invalid());
            }
            for _ in 0..length {
                out.literal(bits.read(8)? as u8)?;
            }
        } else if kind <= 2 {
            let (literals, distances) = trees(&mut bits, kind)?;
            loop {
                let symbol = literals.symbol(&mut bits)?;
                match symbol {
                    0..=255 => out.literal(symbol as u8)?,
                    256 => break,
                    257..=285 => {
                        let index = symbol as usize - 257;
                        let length =
                            u64::from(LENGTH[index]) + u64::from(bits.read(EXTRA[index] as usize)?);
                        let index = distances.symbol(&mut bits)? as usize;
                        if index >= 30 {
                            return Err(invalid());
                        }
                        let distance = u64::from(DISTANCE[index])
                            + u64::from(bits.read(DIST_EXTRA[index] as usize)?);
                        out.copy(length, distance, cancel)?;
                    }
                    _ => return Err(invalid()),
                }
            }
        } else {
            return Err(invalid());
        }
        if last != 0 {
            break;
        }
    }
    if 2 + (bits.position + 7) / 8 + 4 > data.len() || (exact && out.count != maximum) {
        return Err(invalid());
    }
    cancelled(cancel)?;
    Ok((out.count, out.rle.map(RleCount::finish).transpose()?))
}

fn rle(data: &[u8], expected: u64, cancel: &AtomicBool) -> Result<u64> {
    let mut counter = RleCount::new(expected);
    for (index, &byte) in data.iter().enumerate() {
        if index & 4095 == 0 {
            cancelled(cancel)?;
        }
        counter.byte(byte)?;
    }
    counter.finish()
}

#[derive(Default)]
struct Shape {
    count: u64,
    raw: u64,
    plane: u64,
    samples: u64,
    largest_samples: u64,
    dct_blocks: u64,
    b44: u64,
    names: u64,
}
fn shape<'a, C>(
    compression: u8,
    geometry: Geometry,
    channels: C,
    cancel: &AtomicBool,
) -> Result<Shape>
where
    C: Clone + Iterator<Item = Channel<'a>>,
{
    let mut shape = Shape::default();
    if geometry.width == 0 || geometry.height == 0 {
        return Err(invalid());
    }
    let _ = i64::from(geometry.origin_y)
        .checked_add(i64::from(geometry.height))
        .ok_or_else(invalid)?;
    for channel in channels {
        cancelled(cancel)?;
        let bytes = match channel.sample_type {
            0 | 2 => 4,
            1 => 2,
            _ => return Err(invalid()),
        };
        if channel.sampling_x == 0 || channel.sampling_y == 0 {
            return Err(invalid());
        }
        let width = u64::from(geometry.width).div_ceil(u64::from(channel.sampling_x));
        let height = u64::from(geometry.height).div_ceil(u64::from(channel.sampling_y));
        let samples = mul(width, height)?;
        let plane = mul(samples, bytes)?;
        shape.count = add(&[shape.count, 1])?;
        shape.raw = add(&[
            shape.raw,
            mul(
                mul(u64::from(geometry.width), u64::from(geometry.height))?,
                bytes,
            )?,
        ])?;
        shape.plane = add(&[shape.plane, plane])?;
        shape.samples = add(&[shape.samples, samples])?;
        shape.largest_samples = shape.largest_samples.max(samples);
        shape.dct_blocks = shape
            .dct_blocks
            .max(mul(width.div_ceil(8), height.div_ceil(8))?);
        if matches!(compression, 6 | 7) {
            shape.b44 = add(&[
                shape.b44,
                mul(
                    mul(plane, u64::from(channel.sampling_x))?,
                    u64::from(channel.sampling_y),
                )?,
            ])?;
        }
        shape.names = add(&[shape.names, channel.name.len() as u64])?;
    }
    if shape.count == 0 {
        return Err(invalid());
    }
    Ok(shape)
}

fn huffman_header(data: &[u8]) -> Result<()> {
    if data.len() < 20 || u32_at(data, 0)? >= 65537 || u32_at(data, 4)? >= 65537 {
        return Err(invalid());
    }
    if u64::from(u32_at(data, 12)?).div_ceil(8) > (data.len() - 20) as u64 {
        return Err(invalid());
    }
    Ok(())
}

fn rules(data: &[u8], version: u64, cancel: &AtomicBool) -> Result<(usize, u64)> {
    if version < 2 {
        return Ok((0, 25 * 64));
    }
    let length = u16_at(data, 0)? as usize;
    if length < 2 {
        return Err(invalid());
    }
    let rules = data.get(2..length).ok_or_else(invalid)?;
    let (mut position, mut count, mut strings) = (0usize, 0u64, 0u64);
    while position < rules.len() {
        cancelled(cancel)?;
        let start = position;
        while *rules.get(position).ok_or_else(invalid)? != 0 {
            position += 1;
            if position - start > 128 {
                return Err(invalid());
            }
        }
        let flags = *rules.get(position + 1).ok_or_else(invalid)?;
        let sample = *rules.get(position + 2).ok_or_else(invalid)?;
        if flags >> 4 > 3 || (flags >> 2) & 3 == 3 || sample > 2 {
            return Err(invalid());
        }
        count = add(&[count, 1])?;
        // lossy UTF8最多3×字节；Cow增长与owned后缀保守2倍/min8。
        strings = add(&[strings, mul(((position - start) as u64 * 3).max(8), 2)?])?;
        position += 3;
    }
    Ok((length, add(&[mul(count.max(4), 3 * 64)?, strings])?))
}

fn dwa(data: &[u8], raw: u64, shape: &Shape, cancel: &AtomicBool) -> Result<u64> {
    if data.is_empty() {
        return Ok(raw);
    }
    let leader = data.get(..88).ok_or_else(invalid)?;
    let mut header = [0u64; 11];
    for (index, value) in header.iter_mut().enumerate() {
        *value = u64::from_le_bytes(
            leader[index * 8..index * 8 + 8]
                .try_into()
                .map_err(|_| invalid())?,
        );
        if index != 0 && *value > i64::MAX as u64 {
            return Err(invalid());
        }
    }
    if header[10] > 1 {
        return Err(invalid());
    }
    let (rules_length, rule_work) = rules(&data[88..], header[0], cancel)?;
    let mut sections: [&[u8]; 4] = [&[]; 4];
    let mut position = 88usize.checked_add(rules_length).ok_or_else(invalid)?;
    for (section, length) in sections
        .iter_mut()
        .zip([header[2], header[3], header[4], header[5]])
    {
        let length = usize::try_from(length).map_err(|_| invalid())?;
        let end = position.checked_add(length).ok_or_else(invalid)?;
        *section = data.get(position..end).ok_or_else(invalid)?;
        position = end;
    }
    let unknown = header[1];
    let ac = mul(header[8], 2)?;
    let dc = mul(header[9], 2)?;
    let token_bytes = header[6];
    let rle_goal = header[7];
    // 各声明独立计费；不以画布raw大小代替AC/DC/unknown/RLE的初始化hint。
    for hint in [unknown, ac, dc] {
        if hint > MAX_BYTES {
            return Err(budget());
        }
    }
    if rle_goal != 0 && token_bytes > MAX_BYTES {
        return Err(budget());
    }
    if unknown != 0 {
        inflate(sections[0], unknown, true, None, cancel)?;
    }
    let ac_work = if ac == 0 {
        0
    } else if header[10] == 0 {
        huffman_header(sections[1])?;
        add(&[HUFFMAN_WORK, ac])?
    } else {
        inflate(sections[1], ac, true, None, cancel)?;
        add(&[inflate_peak(ac)?, ac])?
    };
    if dc != 0 {
        inflate(sections[2], dc, true, None, cancel)?;
    }
    let dc_work = if dc == 0 {
        0
    } else {
        inflate_peak(dc)?.max(mul(inflate_capacity(dc)?, 3)?)
    };
    let rle_work = if rle_goal == 0 {
        0
    } else {
        let (_, decoded) = inflate(sections[3], token_bytes, true, Some(rle_goal), cancel)?;
        let decoded = decoded.ok_or_else(invalid)?;
        add(&[
            inflate_peak(token_bytes)?,
            growth_peak(rle_goal.min(16384), decoded)?,
            retained_capacity(rle_goal.min(16384), decoded)?,
        ])?
    };
    // ChannelInfo、prefix-map/csc Vec旧新、三套Vec<Vec>/grouped及名字UTF8/prefix副本。
    let metadata = add(&[
        mul(shape.count.max(4), 3 * (48 + 80 + 24 + 4 * 24 + 1))?,
        mul(shape.names, 8)?,
    ])?;
    // 每通道平面/byte-plane复原、所有half样本、当前可能重复slot的3份group样本。
    let planes = add(&[
        mul(shape.plane, 2)?,
        mul(shape.samples, 2)?,
        mul(shape.largest_samples, 6)?,
    ])?;
    // 后端始终分配3×64f32和3bool的全group DCT阵列，不是只分配一个8×8块。
    let dct = mul(shape.dct_blocks, 3 * 64 * 4 + 3)?;
    let unknown_work = if unknown == 0 {
        0
    } else {
        inflate_peak(unknown)?
    };
    add(&[
        rule_work,
        metadata,
        unknown_work,
        ac_work,
        dc_work,
        rle_work,
        planes,
        dct,
        growth_peak(raw, shape.plane)?,
        FIXED,
    ])
}

/// 只面向非deep块。code为容器原始压缩编号；raw等于后端声明的完整块字节数。
/// 本函数不调用任何codec；DEFLATE只观察输出计数，DWA RLE最多用固定32KiB历史。
/// 调用方仍须把完整源/目录/最终图、显式线程stack及SerialWorkspace峰值合计门控。
pub(super) fn inspect<'a, C>(
    compression: u8,
    compressed: &[u8],
    raw_block_bytes: u64,
    geometry: Geometry,
    channels: C,
    cancel: &AtomicBool,
) -> Result<Workspace>
where
    C: Clone + Iterator<Item = Channel<'a>>,
{
    let result = (|| {
        cancelled(cancel)?;
        let input = compressed.len() as u64;
        let raw_path = input == raw_block_bytes || compression == 0;
        let shape = shape(
            if raw_path { 0 } else { compression },
            geometry,
            channels,
            cancel,
        )?;
        if raw_block_bytes != shape.raw {
            return Err(invalid());
        }
        if input > MAX_BYTES {
            return Err(budget());
        }
        // 所有压缩器共同的raw旁路必须先于压缩头解释，包括小块和不可压缩块。
        if raw_path {
            return Ok(Workspace {
                temporary_bytes: add(&[input, FIXED])?,
                interleave_scratch_bytes: 0,
            });
        }
        let raw = raw_block_bytes;
        let mut scratch = 0;
        let extra = match compression {
            1 => {
                let actual = rle(compressed, raw, cancel)?;
                if actual != raw {
                    return Err(invalid());
                }
                scratch = raw;
                add(&[growth_peak(raw.min(16384), raw)?, FIXED])?
            }
            2 | 3 => {
                inflate(compressed, raw, true, None, cancel)?;
                scratch = raw;
                add(&[inflate_peak(raw)?, FIXED])?
            }
            4 => {
                let min = u16_at(compressed, 0)? as usize;
                let max = u16_at(compressed, 2)? as usize;
                if min >= 8192 || max >= 8192 {
                    return Err(invalid());
                }
                let start = 4 + if min <= max { max - min + 1 } else { 0 };
                u32_at(compressed, start)?;
                huffman_header(compressed.get(start + 4..).ok_or_else(invalid)?)?;
                // SmallVec<[ChannelData;6]>收集可能溢出inline并倍增，另留旧新峰。
                add(&[
                    mul(raw, 2)?,
                    HUFFMAN_WORK,
                    mul(shape.count.max(6), 3 * 64)?,
                    FIXED,
                ])?
            }
            5 => {
                inflate(compressed, raw, false, None, cancel)?;
                add(&[
                    inflate_capacity(raw)?,
                    growth_peak(raw.min(8192), raw)?,
                    FIXED,
                ])?
            }
            6 | 7 => add(&[mul(raw.max(shape.b44), 5)?, mul(shape.count, 64)?, FIXED])?,
            8 | 9 => dwa(compressed, raw, &shape, cancel)?,
            _ => return Err(unsupported()),
        };
        Ok(Workspace {
            temporary_bytes: add(&[input, extra])?,
            interleave_scratch_bytes: scratch,
        })
    })();
    cancelled(cancel)?;
    result
}

#[cfg(test)]
#[path = "image_decode_exr_compression_tests.rs"]
mod tests;
