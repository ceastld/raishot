//! 文本图片头的零分配工作区观察；像素、缩放及RLE仍由统一后端处理。
use std::sync::atomic::{AtomicBool, Ordering};

use crate::Result;

const FIXED_BYTES: u64 = 64 * 1024;
const ATTRIBUTE_BYTES: u64 = std::mem::size_of::<(String, String)>() as u64;

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(tr!("图片读取已取消", "Image reading canceled").into())
    } else {
        Ok(())
    }
}

fn invalid() -> Box<dyn std::error::Error> {
    tr!(
        "PNM或HDR图片头无效或被截断",
        "Invalid or truncated PNM or HDR image header"
    )
    .into()
}

fn unsupported() -> Box<dyn std::error::Error> {
    tr!(
        "PNM或HDR头类型不受当前解码器支持",
        "PNM or HDR header type is unsupported by the current decoder"
    )
    .into()
}

fn budget() -> Box<dyn std::error::Error> {
    tr!(
        "文本图片解码工作区超过共享驻留预算",
        "Text image decode workspace exceeds the shared resident budget"
    )
    .into()
}

fn sum(parts: &[u64]) -> Result<u64> {
    parts
        .iter()
        .try_fold(0u64, |total, part| total.checked_add(*part))
        .ok_or_else(budget)
}

/// 记录可见Vec/String请求容量的保守峰值，不把分配器本身开销算作像素容量。
struct Ledger {
    resident: u64,
    peak: u64,
}

impl Ledger {
    fn new(resident: u64) -> Result<Self> {
        let mut value = Self { resident, peak: 0 };
        value.observe(0)?;
        Ok(value)
    }

    fn observe(&mut self, bytes: u64) -> Result<()> {
        let workspace = sum(&[FIXED_BYTES, bytes])?;
        super::check_shared_budget(&[self.resident, workspace])?;
        self.peak = self.peak.max(workspace);
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct Buffer {
    length: u64,
    capacity: u64,
}

impl Buffer {
    /// 单字节push逐次倍增。HDR行的初始容量16、PAM行的最低容量8分别传入。
    fn pushes(&mut self, length: u64, minimum: u64, other: u64, ledger: &mut Ledger) -> Result<()> {
        while self.capacity < length {
            let next = self
                .capacity
                .checked_mul(2)
                .ok_or_else(budget)?
                .max(minimum);
            if next <= self.capacity {
                return Err(budget());
            }
            ledger.observe(sum(&[other, self.capacity, next])?)?;
            self.capacity = next;
        }
        self.length = length;
        ledger.observe(sum(&[other, self.capacity])?)
    }

    /// String::push_str或Vec::push的单次reserve，保留旧、新块同时存在的阶段。
    fn append(
        &mut self,
        length: u64,
        minimum: u64,
        element: u64,
        other: u64,
        ledger: &mut Ledger,
    ) -> Result<()> {
        let needed = sum(&[self.length, length])?;
        if needed > self.capacity {
            let next = self
                .capacity
                .checked_mul(2)
                .ok_or_else(budget)?
                .max(needed)
                .max(minimum);
            let peak = sum(&[self.capacity, next])?
                .checked_mul(element)
                .ok_or_else(budget)?;
            ledger.observe(sum(&[other, peak])?)?;
            self.capacity = next;
        }
        self.length = needed;
        ledger.observe(sum(&[
            other,
            self.capacity.checked_mul(element).ok_or_else(budget)?,
        ])?)
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
    cancel: &'a AtomicBool,
}

impl<'a> Cursor<'a> {
    fn byte(&mut self) -> Result<Option<u8>> {
        if self.position & 4095 == 0 {
            cancelled(self.cancel)?;
        }
        let byte = self.bytes.get(self.position).copied();
        if byte.is_some() {
            self.position += 1;
        }
        Ok(byte)
    }

    fn line(&mut self) -> Result<Option<&'a [u8]>> {
        let start = self.position;
        while let Some(byte) = self.byte()? {
            if byte == b'\n' {
                return Ok(Some(&self.bytes[start..self.position - 1]));
            }
        }
        Ok((self.position != start).then_some(&self.bytes[start..self.position]))
    }

    /// P1–P6按字节流跳过注释，不构造token。允许任意长的前导0。
    fn integer(&mut self) -> Result<u32> {
        let mut enabled = true;
        let mut found = false;
        let mut value = 0u32;
        while let Some(byte) = self.byte()? {
            let current = enabled && byte != b'#';
            enabled = current || matches!(byte, b'\r' | b'\n');
            if !current {
                continue;
            }
            if byte.is_ascii_whitespace() || matches!(byte, 0x0b | 0x0c) {
                if found {
                    break;
                }
            } else if byte.is_ascii_digit() {
                value = value
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(u32::from(byte - b'0')))
                    .ok_or_else(invalid)?;
                found = true;
            } else {
                return Err(invalid());
            }
        }
        if found {
            Ok(value)
        } else {
            Err(invalid())
        }
    }
}

fn layout(
    dimensions: (u32, u32),
    bytes_per_pixel: u64,
    constructor: u64,
    extra: u64,
) -> Result<super::CodecWorkspace> {
    let native_bytes = u64::from(dimensions.0)
        .checked_mul(u64::from(dimensions.1))
        .and_then(|pixels| pixels.checked_mul(bytes_per_pixel))
        .ok_or_else(budget)?;
    super::check_dimensions(dimensions, native_bytes)?;
    Ok(super::CodecWorkspace {
        dimensions,
        native_bytes,
        constructor_bytes: constructor,
        decode_extra_bytes: extra,
        // 两种后端均沿默认set_limits，只检查尺寸，不另reserve私有缓冲。
        backend_limit_floor: 0,
    })
}

fn whitespace(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ')
}

fn trim_ascii<'a>(mut bytes: &'a [u8], cancel: &AtomicBool) -> Result<&'a [u8]> {
    let mut count = 0usize;
    while bytes.first().is_some_and(|&byte| whitespace(byte)) {
        if count & 4095 == 0 {
            cancelled(cancel)?;
        }
        bytes = &bytes[1..];
        count += 1;
    }
    while bytes.last().is_some_and(|&byte| whitespace(byte)) {
        if count & 4095 == 0 {
            cancelled(cancel)?;
        }
        bytes = &bytes[..bytes.len() - 1];
        count += 1;
    }
    Ok(bytes)
}

fn ascii_number(bytes: &[u8], cancel: &AtomicBool) -> Result<u32> {
    let mut bytes = trim_ascii(bytes, cancel)?;
    if bytes.first() == Some(&b'+') {
        bytes = &bytes[1..];
    }
    if bytes.is_empty() {
        return Err(invalid());
    }
    let mut value = 0u32;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if index & 4095 == 0 {
            cancelled(cancel)?;
        }
        if !byte.is_ascii_digit() {
            return Err(invalid());
        }
        value = value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or_else(invalid)?;
    }
    Ok(value)
}

fn set_integer(target: &mut Option<u32>, text: &[u8], cancel: &AtomicBool) -> Result<()> {
    if target.is_some() {
        return Err(invalid());
    }
    // PAM沿用str::parse，保留leading +与任意长前导0的行为。
    *target = Some(ascii_number(text, cancel)?);
    Ok(())
}

fn pam(cursor: &mut Cursor<'_>, ledger: &mut Ledger) -> Result<super::CodecWorkspace> {
    if cursor.byte()? != Some(b'\n') {
        return Err(invalid());
    }
    let mut width = None;
    let mut height = None;
    let mut depth = None;
    let mut maxval = None;
    let mut previous_line = 0;
    let mut tuple = Buffer::default();
    let mut tuple_text = None;
    let mut tuple_count = 0u64;
    loop {
        let raw = cursor.line()?.ok_or_else(invalid)?;
        let mut line_buffer = Buffer::default();
        // `line = read_next_line()`在右侧读取完成后才丢弃上一行。
        line_buffer.pushes(
            raw.len() as u64,
            8,
            sum(&[previous_line, tuple.capacity])?,
            ledger,
        )?;
        previous_line = line_buffer.capacity;
        if lossy_length(raw, cursor.cancel)?.1 {
            return Err(invalid());
        }
        if raw.is_empty() {
            return Err(invalid());
        }
        if raw[0] == b'#' {
            continue;
        }
        for part in raw.chunks(4096) {
            cancelled(cursor.cancel)?;
            if !part.is_ascii() {
                return Err(invalid());
            }
        }
        // 后端先trim_left却沿原行的空白位置分割；leading空白会产生空identifier。
        if whitespace(raw[0]) {
            return Err(invalid());
        }
        let mut split = raw.len();
        for (index, byte) in raw.iter().copied().enumerate() {
            if index & 4095 == 0 {
                cancelled(cursor.cancel)?;
            }
            if whitespace(byte) {
                split = index;
                break;
            }
        }
        let identifier = &raw[..split];
        let rest = &raw[split..];
        match identifier {
            b"ENDHDR" => break,
            b"WIDTH" => set_integer(&mut width, rest, cursor.cancel)?,
            b"HEIGHT" => set_integer(&mut height, rest, cursor.cancel)?,
            b"DEPTH" => set_integer(&mut depth, rest, cursor.cancel)?,
            b"MAXVAL" => set_integer(&mut maxval, rest, cursor.cancel)?,
            b"TUPLTYPE" => {
                let value = trim_ascii(rest, cursor.cancel)?;
                if tuple_count == 0 {
                    // 初次to_owned是精确长度；随后push(' ')/push_str分别可能扩容。
                    tuple = Buffer {
                        length: value.len() as u64,
                        capacity: value.len() as u64,
                    };
                    ledger.observe(sum(&[line_buffer.capacity, tuple.capacity])?)?;
                    tuple_text = Some(value);
                } else {
                    tuple.append(1, 8, 1, line_buffer.capacity, ledger)?;
                    tuple.append(value.len() as u64, 8, 1, line_buffer.capacity, ledger)?;
                }
                tuple_count = tuple_count.checked_add(1).ok_or_else(budget)?;
            }
            _ => return Err(invalid()),
        }
    }
    let (width, height, depth, maxval) = match (width, height, depth, maxval) {
        (Some(width), Some(height), Some(depth), Some(maxval)) => (width, height, depth, maxval),
        _ => return Err(invalid()),
    };
    if maxval == 0 {
        return Err(invalid());
    }
    let bytes_per_pixel = match (tuple_count, tuple_text) {
        (0, _) if (1..=4).contains(&depth) => u64::from(depth),
        (1, Some(b"BLACKANDWHITE")) if depth == 1 && maxval == 1 => 1,
        (1, Some(b"BLACKANDWHITE_ALPHA")) if depth == 2 && maxval == 1 => 2,
        (1, Some(b"GRAYSCALE")) if depth == 1 && maxval <= 255 => 1,
        // 保留现后端的depth<=1条件及无TUPLTYPE时固定U8的选择。
        (1, Some(b"GRAYSCALE")) if depth <= 1 && maxval <= 65535 => 2,
        (1, Some(b"GRAYSCALE_ALPHA")) if depth == 2 && maxval <= 255 => 2,
        (1, Some(b"GRAYSCALE_ALPHA")) if depth == 2 && maxval <= 65535 => 4,
        (1, Some(b"RGB")) if depth == 3 && maxval <= 255 => 3,
        (1, Some(b"RGB")) if depth == 3 && maxval <= 65535 => 6,
        (1, Some(b"RGB_ALPHA")) if depth == 4 && maxval <= 255 => 4,
        (1, Some(b"RGB_ALPHA")) if depth == 4 && maxval <= 65535 => 8,
        _ => return Err(unsupported()),
    };
    // 已知tuple转换为枚举后，行/tuple String均释放；encoded头为None。
    layout((width, height), bytes_per_pixel, ledger.peak, FIXED_BYTES)
}

fn pnm(cursor: &mut Cursor<'_>, ledger: &mut Ledger) -> Result<super::CodecWorkspace> {
    let kind = cursor.bytes[1];
    cursor.position = 2;
    if kind == b'7' {
        return pam(cursor, ledger);
    }
    let dimensions = (cursor.integer()?, cursor.integer()?);
    let bytes_per_pixel = if matches!(kind, b'1' | b'4') {
        1
    } else {
        let maxval = cursor.integer()?;
        if !(1..=65535).contains(&maxval) {
            return Err(invalid());
        }
        let components = if matches!(kind, b'3' | b'6') { 3 } else { 1 };
        components * if maxval <= 255 { 1 } else { 2 }
    };
    // ASCII数字不生成String；PBM binary只用caller的packed前段原地反向展开。
    layout(dimensions, bytes_per_pixel, ledger.peak, FIXED_BYTES)
}

/// 只计from_utf8_lossy的输出长度，不分配替换后的字符串。
fn lossy_length(bytes: &[u8], cancel: &AtomicBool) -> Result<(u64, bool)> {
    let mut length = 0u64;
    let mut invalid_utf8 = false;
    let mut position = 0;
    while position < bytes.len() {
        cancelled(cancel)?;
        let end = position.saturating_add(4096).min(bytes.len());
        match std::str::from_utf8(&bytes[position..end]) {
            Ok(text) => {
                length = sum(&[length, text.len() as u64])?;
                position = end;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                length = sum(&[length, valid as u64])?;
                position += valid;
                if let Some(invalid) = error.error_len() {
                    invalid_utf8 = true;
                    length = sum(&[length, 3])?;
                    position += invalid;
                } else if end == bytes.len() {
                    invalid_utf8 = true;
                    length = sum(&[length, 3])?;
                    position = end;
                }
                // 分片末尾未完整的UTF8序列留给下一片，不提前生成替换字符。
            }
        }
    }
    Ok((length, invalid_utf8))
}

fn raw_line_buffer(length: u64, other: u64, ledger: &mut Ledger) -> Result<Buffer> {
    let mut buffer = Buffer {
        length: 0,
        capacity: 16,
    };
    ledger.observe(sum(&[other, 16])?)?;
    buffer.pushes(length, 16, other, ledger)?;
    Ok(buffer)
}

/// 分片读取维度行里的Unicode空白；不会为整行构造String或lossy副本。
fn hdr_token<'a>(
    bytes: &'a [u8],
    position: &mut usize,
    cancel: &AtomicBool,
) -> Result<Option<&'a [u8]>> {
    let mut start = None;
    while *position < bytes.len() {
        // 字符最多4B，越过分片边界后的前三个位置同样检查。
        if *position & 4095 < 4 {
            cancelled(cancel)?;
        }
        let width = match bytes[*position] {
            0..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Err(invalid()),
        };
        let raw = bytes
            .get(*position..*position + width)
            .ok_or_else(invalid)?;
        let scalar = std::str::from_utf8(raw)
            .map_err(|_| invalid())?
            .chars()
            .next()
            .ok_or_else(invalid)?;
        if scalar.is_whitespace() {
            if let Some(start) = start {
                return Ok(Some(&bytes[start..*position]));
            }
        } else if start.is_none() {
            start = Some(*position);
        }
        *position += width;
    }
    Ok(start.map(|start| &bytes[start..*position]))
}

fn hdr(cursor: &mut Cursor<'_>, ledger: &mut Ledger) -> Result<super::CodecWorkspace> {
    cursor.position = 10;
    let signature_tail = cursor.line()?.ok_or_else(invalid)?;
    raw_line_buffer(signature_tail.len() as u64, 0, ledger)?;
    let mut attributes = Buffer::default();
    let mut strings = 0u64;
    loop {
        let raw = cursor.line()?.ok_or_else(invalid)?;
        let retained = sum(&[
            attributes
                .capacity
                .checked_mul(ATTRIBUTE_BYTES)
                .ok_or_else(budget)?,
            strings,
        ])?;
        let line = raw_line_buffer(raw.len() as u64, retained, ledger)?;
        if raw.is_empty() {
            break;
        }
        if raw[0] == b'#' {
            continue;
        }
        let (length, lossy) = lossy_length(raw, cursor.cancel)?;
        let mut cow = Buffer::default();
        if lossy {
            // lossy既有逐字符也有整段append，容量不能锚定输入长度做倍增。
            // 每次增长前旧cap<最终length，新cap<=2*length（另有最小8B）；
            // 保留态2U、旧新过渡3U分别覆盖所有切块方式及UTF8替换放大。
            let upper = length.max(8);
            cow.capacity = upper.checked_mul(2).ok_or_else(budget)?;
            let growth = upper.checked_mul(3).ok_or_else(budget)?;
            ledger.observe(sum(&[retained, line.capacity, growth])?)?;
        }
        // key.trim()+value的owned长度不会超过整条lossy长度。连同分隔符计费，
        // 保守覆盖Unicode空白与损坏UTF8；不另造字符串去复制backend属性语义。
        // FORMAT/float属性合法性仍交原decoder，错误消息仅保留有界片段。
        strings = sum(&[strings, length])?;
        attributes.append(
            1,
            4,
            ATTRIBUTE_BYTES,
            sum(&[strings, line.capacity, cow.capacity])?,
            ledger,
        )?;
    }
    let retained = sum(&[
        attributes
            .capacity
            .checked_mul(ATTRIBUTE_BYTES)
            .ok_or_else(budget)?,
        strings,
    ])?;
    let raw = cursor.line()?.ok_or_else(invalid)?;
    raw_line_buffer(raw.len() as u64, retained, ledger)?;
    let mut position = 0;
    let first = hdr_token(raw, &mut position, cursor.cancel)?.ok_or_else(invalid)?;
    let height = hdr_token(raw, &mut position, cursor.cancel)?.ok_or_else(invalid)?;
    let second = hdr_token(raw, &mut position, cursor.cancel)?.ok_or_else(invalid)?;
    let width = hdr_token(raw, &mut position, cursor.cancel)?.ok_or_else(invalid)?;
    if hdr_token(raw, &mut position, cursor.cancel)?.is_some() {
        return Err(invalid());
    }
    if (first, second) != (b"-Y".as_slice(), b"+X".as_slice()) {
        return Err(unsupported());
    }
    let dimensions = (
        ascii_number(width, cursor.cancel)?,
        ascii_number(height, cursor.cancel)?,
    );
    let row = u64::from(dimensions.0).checked_mul(4).ok_or_else(budget)?;
    // 原始/旧RLE/新分量RLE均仅保一条RGBE8行，新RLE另用128B固定栈。
    layout(
        dimensions,
        12,
        ledger.peak,
        sum(&[FIXED_BYTES, retained, row])?,
    )
}

/// resident包含来源actual capacity及宿主R，不重复计入来源、caller native或通用元数据池。
/// magic非PNM/HDR返回None；头扫描不分配，不解析像素opcode或改变原backend量化。
pub(super) fn inspect(
    bytes: &[u8],
    resident: u64,
    cancel: &AtomicBool,
) -> Result<Option<super::CodecWorkspace>> {
    let result = (|| {
        cancelled(cancel)?;
        let pnm_magic = bytes.len() >= 2 && bytes[0] == b'P' && matches!(bytes[1], b'1'..=b'7');
        let hdr_magic = bytes.starts_with(b"#?RADIANCE");
        if !pnm_magic && !hdr_magic {
            return Ok(None);
        }
        if resident < bytes.len() as u64 {
            return Err(budget());
        }
        let mut ledger = Ledger::new(resident)?;
        let mut cursor = Cursor {
            bytes,
            position: 0,
            cancel,
        };
        let workspace = if pnm_magic {
            pnm(&mut cursor, &mut ledger)?
        } else {
            hdr(&mut cursor, &mut ledger)?
        };
        super::check_shared_budget(&[resident, workspace.constructor_bytes])?;
        super::check_shared_budget(&[
            resident,
            workspace.native_bytes,
            workspace.decode_extra_bytes,
        ])?;
        Ok(Some(workspace))
    })();
    cancelled(cancel)?;
    result
}

#[cfg(test)]
#[path = "image_decode_text_budget_tests.rs"]
mod tests;
