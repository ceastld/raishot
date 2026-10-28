//! 静态HEVC项目的有界码流预检；检查编码画布，不执行像素解码。
//! 首批接受单层Main/Main10/Main Still Picture、单个IDR画面的受控参数集。
//! DPB声明只参与可见缓冲预算，不代表系统解码器私有堆的峰值保证。
use super::{
    budget, check_cancel, checked_pixels, invalid, unsupported, ByteView, Cicp, CodedColor,
    CodedInfo, PlatformError, SampleModel,
};
use std::sync::atomic::AtomicBool;

const MAX_PARTS: usize = 4096;
const MAX_PARAMETER_BYTES: usize = 1024 * 1024;
const MAX_PARAMETER_NAL: usize = 65_535;
const MAX_INPUT_BYTES: usize = 256 * 1024 * 1024;

fn malformed() -> PlatformError {
    invalid(
        "HEVC码流头无效或被截断",
        "Invalid or truncated HEVC bitstream header",
    )
}

fn outside_subset() -> PlatformError {
    unsupported(
        "HEVC码流结构超出当前受控静态子集",
        "HEVC bitstream structure is outside the controlled still-image subset",
    )
}

#[derive(Clone, Copy)]
struct Header {
    kind: u8,
}

fn header(nal: ByteView<'_>) -> Result<Header, PlatformError> {
    if nal.len() < 2 {
        return Err(malformed());
    }
    let first = nal.byte(0)?;
    let second = nal.byte(1)?;
    if first & 0x80 != 0 || second & 7 == 0 {
        return Err(malformed());
    }
    if first & 1 != 0 || second >> 3 != 0 || second & 7 != 1 {
        return Err(outside_subset());
    }
    Ok(Header {
        kind: (first >> 1) & 0x3f,
    })
}

/// 在线移除RBSP防竞争字节，不为参数集或整幅编码负载复制缓冲。
struct Bits<'a, 'c> {
    view: ByteView<'a>,
    cancel: &'c AtomicBool,
    pos: usize,
    zeros: u8,
    current: u8,
    left: u8,
}

impl<'a, 'c> Bits<'a, 'c> {
    fn new(nal: ByteView<'a>, cancel: &'c AtomicBool) -> Result<Self, PlatformError> {
        Ok(Self {
            view: nal.slice(2..nal.len())?,
            cancel,
            pos: 0,
            zeros: 0,
            current: 0,
            left: 0,
        })
    }

    fn next_byte(&mut self) -> Result<u8, PlatformError> {
        check_cancel(self.cancel)?;
        let mut byte = self.view.byte(self.pos)?;
        self.pos = self.pos.checked_add(1).ok_or_else(malformed)?;
        if self.zeros == 2 {
            if byte == 3 {
                byte = self.view.byte(self.pos)?;
                self.pos = self.pos.checked_add(1).ok_or_else(malformed)?;
                if byte > 3 {
                    return Err(malformed());
                }
                self.zeros = 0;
            } else if byte < 3 {
                return Err(malformed());
            }
        }
        self.zeros = if byte == 0 { self.zeros + 1 } else { 0 };
        Ok(byte)
    }

    fn read(&mut self, count: u8) -> Result<u32, PlatformError> {
        if count > 32 {
            return Err(malformed());
        }
        let mut value = 0;
        for _ in 0..count {
            if self.left == 0 {
                self.current = self.next_byte()?;
                self.left = 8;
            }
            self.left -= 1;
            value = (value << 1) | u32::from((self.current >> self.left) & 1);
        }
        Ok(value)
    }

    fn flag(&mut self) -> Result<bool, PlatformError> {
        Ok(self.read(1)? != 0)
    }

    fn ue(&mut self, maximum: u32) -> Result<u32, PlatformError> {
        let mut zeros = 0u8;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return Err(malformed());
            }
        }
        let value = ((1u64 << zeros) - 1) + u64::from(self.read(zeros)?);
        if value > u64::from(maximum) {
            return Err(malformed());
        }
        Ok(value as u32)
    }

    fn se(&mut self, minimum: i32, maximum: i32) -> Result<i32, PlatformError> {
        let code = self.ue(65_535)?;
        let value = if code & 1 == 0 {
            -(i64::from(code) / 2)
        } else {
            (i64::from(code) + 1) / 2
        };
        if value < i64::from(minimum) || value > i64::from(maximum) {
            return Err(malformed());
        }
        Ok(value as i32)
    }

    fn finish(mut self) -> Result<(), PlatformError> {
        if !self.flag()? {
            return Err(malformed());
        }
        while self.left != 0 {
            if self.flag()? {
                return Err(malformed());
            }
        }
        if self.pos != self.view.len() {
            return Err(malformed());
        }
        check_cancel(self.cancel)
    }
}

fn ptl(bits: &mut Bits<'_, '_>) -> Result<[u8; 12], PlatformError> {
    let mut bytes = [0; 12];
    for byte in &mut bytes {
        *byte = bits.read(8)? as u8;
    }
    if bytes[0] >> 6 != 0 || !matches!(bytes[0] & 31, 1..=3) {
        return Err(outside_subset());
    }
    Ok(bytes)
}

#[derive(Clone, Copy)]
struct Vps {
    id: u8,
    ptl: [u8; 12],
    buffers: u8,
}

fn ordering(bits: &mut Bits<'_, '_>) -> Result<u8, PlatformError> {
    bits.flag()?; // 单时间子层，两种ordering标志都只含一个三元组。
    let buffers = bits.ue(15)? + 1;
    if bits.ue(15)? >= buffers {
        return Err(malformed());
    }
    bits.ue(u32::MAX - 1)?;
    Ok(buffers as u8)
}

fn parse_vps(nal: ByteView<'_>, cancel: &AtomicBool) -> Result<Vps, PlatformError> {
    let mut bits = Bits::new(nal, cancel)?;
    let id = bits.read(4)? as u8;
    if bits.read(2)? != 3 || bits.read(6)? != 0 || bits.read(3)? != 0 {
        return Err(outside_subset());
    }
    if !bits.flag()? || bits.read(16)? != 0xffff {
        return Err(malformed());
    }
    let ptl = ptl(&mut bits)?;
    let buffers = ordering(&mut bits)?;
    if bits.read(6)? != 0 || bits.ue(1023)? != 0 || bits.flag()? || bits.flag()? {
        return Err(outside_subset());
    }
    bits.finish()?;
    Ok(Vps { id, ptl, buffers })
}

#[derive(Clone, Copy)]
struct Sps {
    id: u32,
    vps_id: u8,
    ptl: [u8; 12],
    coded: (u32, u32),
    display: (u32, u32),
    display_origin: (u32, u32),
    depth: u8,
    chroma: u8,
    buffers: u8,
    cicp: Option<Cicp>,
}

fn vui(bits: &mut Bits<'_, '_>) -> Result<Option<Cicp>, PlatformError> {
    if bits.flag()? && bits.read(8)? == 255 {
        if bits.read(16)? == 0 || bits.read(16)? == 0 {
            return Err(malformed());
        }
    }
    if bits.flag()? {
        bits.flag()?;
    }
    let mut cicp = None;
    if bits.flag()? {
        bits.read(3)?;
        let full_range = bits.flag()?;
        let (primaries, transfer, matrix) = if bits.flag()? {
            (
                bits.read(8)? as u16,
                bits.read(8)? as u16,
                bits.read(8)? as u16,
            )
        } else {
            // video_signal_type存在而颜色描述缺省时，保留范围与语法推导的未指定值。
            (2, 2, 2)
        };
        cicp = Some(Cicp {
            primaries,
            transfer,
            matrix,
            full_range,
        });
    }
    if bits.flag()? {
        bits.ue(5)?;
        bits.ue(5)?;
    }
    bits.flag()?;
    if bits.flag()? {
        return Err(outside_subset()); // 不接受场序列。
    }
    bits.flag()?;
    if bits.flag()? {
        return Err(outside_subset()); // 额外默认显示窗口尚不与ispe混用。
    }
    if bits.flag()? {
        if bits.read(32)? == 0 || bits.read(32)? == 0 {
            return Err(malformed());
        }
        if bits.flag()? {
            bits.ue(u32::MAX - 1)?;
        }
        if bits.flag()? {
            return Err(outside_subset()); // 暂不解释HRD可变数组。
        }
    }
    if bits.flag()? {
        bits.flag()?;
        bits.flag()?;
        bits.flag()?;
        bits.ue(4095)?;
        bits.ue(16)?;
        bits.ue(16)?;
        bits.ue(15)?;
        bits.ue(15)?;
    }
    Ok(cicp)
}

fn short_term_sets(bits: &mut Bits<'_, '_>, count: u32, buffers: u8) -> Result<(), PlatformError> {
    let maximum = u32::from(buffers - 1);
    for index in 0..count {
        if index != 0 && bits.flag()? {
            return Err(outside_subset()); // 暂不解释跨RPS预测。
        }
        let negative = bits.ue(maximum)?;
        let positive = bits.ue(maximum)?;
        let total = negative.checked_add(positive).ok_or_else(malformed)?;
        if total > maximum {
            return Err(malformed());
        }
        for _ in 0..total {
            bits.ue(32_767)?;
            bits.flag()?;
        }
    }
    Ok(())
}

fn parse_sps(
    nal: ByteView<'_>,
    declared: (u32, u32),
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Sps, PlatformError> {
    let mut bits = Bits::new(nal, cancel)?;
    let vps_id = bits.read(4)? as u8;
    if bits.read(3)? != 0 {
        return Err(outside_subset());
    }
    if !bits.flag()? {
        return Err(malformed());
    }
    let ptl = ptl(&mut bits)?;
    let id = bits.ue(15)?;
    let chroma = bits.ue(3)? as u8;
    if chroma != 1 {
        return Err(outside_subset());
    }
    let coded = (bits.ue(u32::MAX - 1)?, bits.ue(u32::MAX - 1)?);
    checked_pixels(coded, MAX_INPUT_BYTES as u64)?;
    let mut crop = [0u32; 4];
    if bits.flag()? {
        for value in &mut crop {
            *value = bits.ue(u32::MAX - 1)?;
        }
    }
    let crop_x = crop[0]
        .checked_add(crop[1])
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(malformed)?;
    let crop_y = crop[2]
        .checked_add(crop[3])
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(malformed)?;
    let display = (
        coded.0.checked_sub(crop_x).ok_or_else(malformed)?,
        coded.1.checked_sub(crop_y).ok_or_else(malformed)?,
    );
    let display_origin = (
        crop[0].checked_mul(2).ok_or_else(malformed)?,
        crop[2].checked_mul(2).ok_or_else(malformed)?,
    );
    if display.0 == 0 || display.1 == 0 || display != declared {
        return Err(invalid(
            "HEVC编码裁切尺寸与容器声明不一致",
            "HEVC conformance-window dimensions do not match the container",
        ));
    }
    checked_pixels(display, max_output_bytes)?;
    let depth = bits.ue(8)? + 8;
    let chroma_depth = bits.ue(8)? + 8;
    if depth != chroma_depth || !matches!(depth, 8 | 10) {
        return Err(outside_subset());
    }
    if matches!(ptl[0] & 31, 1 | 3) && depth != 8 {
        return Err(malformed());
    }
    bits.ue(12)?;
    let buffers = ordering(&mut bits)?;
    let min_cb = bits.ue(3)? + 3;
    let max_cb = min_cb + bits.ue(3)?;
    let min_transform = bits.ue(3)? + 2;
    let max_transform = min_transform + bits.ue(3)?;
    if max_cb > 6
        || max_transform > 5
        || min_transform > min_cb
        || max_transform > max_cb
        || coded.0 % (1 << min_cb) != 0
        || coded.1 % (1 << min_cb) != 0
    {
        return Err(malformed());
    }
    bits.ue(5)?;
    bits.ue(5)?;
    if bits.flag()? && bits.flag()? {
        return Err(outside_subset()); // 不分配或解释自定义量化矩阵。
    }
    bits.flag()?;
    bits.flag()?;
    if bits.flag()? {
        return Err(outside_subset());
    }
    let sets = bits.ue(64)?;
    short_term_sets(&mut bits, sets, buffers)?;
    if bits.flag()? {
        return Err(outside_subset()); // 长期参考图片集后续单独扩展。
    }
    bits.flag()?;
    bits.flag()?;
    let cicp = if bits.flag()? { vui(&mut bits)? } else { None };
    if bits.flag()? {
        return Err(outside_subset());
    }
    bits.finish()?;
    Ok(Sps {
        id,
        vps_id,
        ptl,
        coded,
        display,
        display_origin,
        depth: depth as u8,
        chroma,
        buffers,
        cicp,
    })
}

#[derive(Clone, Copy)]
struct Pps {
    id: u32,
    sps_id: u32,
}

fn parse_pps(nal: ByteView<'_>, cancel: &AtomicBool) -> Result<Pps, PlatformError> {
    let mut bits = Bits::new(nal, cancel)?;
    let id = bits.ue(63)?;
    let sps_id = bits.ue(15)?;
    bits.flag()?;
    bits.flag()?;
    bits.read(3)?;
    bits.flag()?;
    bits.flag()?;
    bits.ue(14)?;
    bits.ue(14)?;
    bits.se(-64, 25)?;
    bits.flag()?;
    bits.flag()?;
    if bits.flag()? {
        bits.ue(3)?;
    }
    bits.se(-12, 12)?;
    bits.se(-12, 12)?;
    bits.flag()?;
    bits.flag()?;
    bits.flag()?;
    bits.flag()?;
    let tiles = bits.flag()?;
    bits.flag()?;
    if tiles {
        return Err(outside_subset());
    }
    bits.flag()?;
    if bits.flag()? {
        bits.flag()?;
        if !bits.flag()? {
            bits.se(-6, 6)?;
            bits.se(-6, 6)?;
        }
    }
    if bits.flag()? {
        return Err(outside_subset());
    }
    bits.flag()?;
    bits.ue(4)?;
    if bits.flag()? || bits.flag()? {
        return Err(outside_subset());
    }
    bits.finish()?;
    Ok(Pps { id, sps_id })
}

fn equal(
    left: ByteView<'_>,
    right: ByteView<'_>,
    cancel: &AtomicBool,
) -> Result<bool, PlatformError> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for index in 0..left.len() {
        if index & 4095 == 0 {
            check_cancel(cancel)?;
        }
        if left.byte(index)? != right.byte(index)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn word(view: ByteView<'_>, offset: usize, size: usize) -> Result<usize, PlatformError> {
    let end = offset.checked_add(size).ok_or_else(malformed)?;
    let view = view.slice(offset..end)?;
    let mut result = 0usize;
    for index in 0..size {
        let byte = view.byte(index)?;
        result = result
            .checked_mul(256)
            .and_then(|value| value.checked_add(usize::from(byte)))
            .ok_or_else(malformed)?;
    }
    Ok(result)
}

fn metadata_bytes(total: &mut usize, nal: ByteView<'_>) -> Result<(), PlatformError> {
    *total = total.checked_add(nal.len()).ok_or_else(malformed)?;
    if *total > MAX_PARAMETER_BYTES {
        return Err(budget());
    }
    Ok(())
}

pub(super) fn inspect(
    config: &[u8],
    data: ByteView<'_>,
    declared: (u32, u32),
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<CodedInfo, PlatformError> {
    check_cancel(cancel)?;
    if config.len() < 23 || config[0] != 1 {
        return Err(malformed());
    }
    if config.len() > MAX_PARAMETER_BYTES || data.len() > MAX_INPUT_BYTES {
        return Err(budget());
    }
    if config[13] & 0xf0 != 0xf0
        || config[15] & 0xfc != 0xfc
        || config[16] & 0xfc != 0xfc
        || config[17] & 0xf8 != 0xf8
        || config[18] & 0xf8 != 0xf8
    {
        return Err(malformed());
    }
    if config[21] & 3 != 3 || (config[21] >> 3) & 7 > 1 {
        return Err(outside_subset());
    }
    let config_view = ByteView::from_slice(config);
    let mut parameters = [None; 3];
    let mut cursor = 23usize;
    let mut parts = 0usize;
    let mut metadata = 0usize;
    for _ in 0..config[22] {
        check_cancel(cancel)?;
        let array = config_view.byte(cursor)?;
        if array & 0x40 != 0 {
            return Err(malformed());
        }
        let kind = array & 0x3f;
        let count = word(config_view, cursor.checked_add(1).ok_or_else(malformed)?, 2)?;
        cursor = cursor.checked_add(3).ok_or_else(malformed)?;
        if !matches!(kind, 32..=34 | 39 | 40) {
            return Err(outside_subset());
        }
        for _ in 0..count {
            check_cancel(cancel)?;
            parts = parts.checked_add(1).ok_or_else(malformed)?;
            if parts > MAX_PARTS {
                return Err(budget());
            }
            let length = word(config_view, cursor, 2)?;
            cursor = cursor.checked_add(2).ok_or_else(malformed)?;
            let end = cursor.checked_add(length).ok_or_else(malformed)?;
            let nal = config_view.slice(cursor..end)?;
            cursor = end;
            metadata_bytes(&mut metadata, nal)?;
            if header(nal)?.kind != kind {
                return Err(malformed());
            }
            if (32..=34).contains(&kind) {
                if nal.len() > MAX_PARAMETER_NAL {
                    return Err(budget());
                }
                let slot = &mut parameters[usize::from(kind - 32)];
                if slot.is_some() {
                    return Err(outside_subset());
                }
                *slot = Some(nal);
            }
        }
    }
    if cursor != config.len() {
        return Err(malformed());
    }
    let vps_nal = parameters[0].ok_or_else(outside_subset)?;
    let sps_nal = parameters[1].ok_or_else(outside_subset)?;
    let pps_nal = parameters[2].ok_or_else(outside_subset)?;
    let vps = parse_vps(vps_nal, cancel)?;
    let sps = parse_sps(sps_nal, declared, max_output_bytes, cancel)?;
    let pps = parse_pps(pps_nal, cancel)?;
    if vps.ptl != sps.ptl || config[1..13] != sps.ptl {
        // 首批不解释不同参数集之间的profile/level聚合，仅接受完全一致的配置。
        return Err(outside_subset());
    }
    if vps.id != sps.vps_id
        || pps.sps_id != sps.id
        || sps.buffers > vps.buffers
        || config[16] & 3 != sps.chroma
        || config[17] & 7 != sps.depth - 8
        || config[18] & 7 != sps.depth - 8
    {
        return Err(invalid(
            "HEVC配置与实际参数集不一致",
            "HEVC configuration does not match its parameter sets",
        ));
    }
    cursor = 0;
    let mut picture_kind = None;
    while cursor < data.len() {
        check_cancel(cancel)?;
        parts = parts.checked_add(1).ok_or_else(malformed)?;
        if parts > MAX_PARTS {
            return Err(budget());
        }
        let length = word(data, cursor, 4)?;
        cursor = cursor.checked_add(4).ok_or_else(malformed)?;
        let end = cursor.checked_add(length).ok_or_else(malformed)?;
        let nal = data.slice(cursor..end)?;
        cursor = end;
        let kind = header(nal)?.kind;
        match kind {
            32..=34 => {
                metadata_bytes(&mut metadata, nal)?;
                if !equal(nal, parameters[usize::from(kind - 32)].unwrap(), cancel)? {
                    return Err(outside_subset());
                }
            }
            19 | 20 => {
                let mut bits = Bits::new(nal, cancel)?;
                let first_slice = bits.flag()?;
                bits.flag()?; // IDR的no_output_of_prior_pics_flag。
                if bits.ue(63)? != pps.id {
                    return Err(malformed());
                }
                match picture_kind {
                    None if first_slice => picture_kind = Some(kind),
                    Some(previous) if !first_slice && previous == kind => {}
                    _ => return Err(outside_subset()),
                }
            }
            35 => {
                metadata_bytes(&mut metadata, nal)?;
                let mut bits = Bits::new(nal, cancel)?;
                if picture_kind.is_some() || bits.read(3)? > 2 {
                    return Err(outside_subset());
                }
                bits.finish()?;
            }
            39 | 40 => metadata_bytes(&mut metadata, nal)?,
            _ => return Err(outside_subset()),
        }
    }
    if picture_kind.is_none() {
        return Err(malformed());
    }
    check_cancel(cancel)?;
    Ok(CodedInfo {
        coded_dimensions: sps.coded,
        display_dimensions: sps.display,
        display_origin: sps.display_origin,
        bits_per_channel: sps.depth,
        picture_buffers: sps.buffers,
        color: CodedColor {
            model: SampleModel::Ycc,
            cicp: sps.cicp,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEIC: &[u8] =
        include_bytes!("../tests/data/image_platform/hevc-confwin-420-8bit-left.heic");
    const LIMIT: u64 = 256 * 1024 * 1024;

    fn token() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn fixture() -> (&'static [u8], &'static [u8]) {
        assert_eq!(HEIC.len(), 4545);
        assert_eq!(&HEIC[0xa4..0xac], b"\0\0\0qhvcC");
        assert_eq!(&HEIC[0x163..0x16b], b"\0\0\x10^mdat");
        (&HEIC[0xac..0x115], &HEIC[0x16b..])
    }

    fn real_nals() -> [Vec<u8>; 3] {
        [
            HEIC[0xc8..0xe0].to_vec(),
            HEIC[0xe5..0x109].to_vec(),
            HEIC[0x10e..0x115].to_vec(),
        ]
    }

    fn configuration(nals: &[Vec<u8>; 3], depth: u8) -> Vec<u8> {
        let (base, _) = fixture();
        let mut result = base[..23].to_vec();
        result[1] = if depth == 10 { 2 } else { 1 };
        if depth == 10 {
            result[2] = 0x20;
        }
        result[17] = 0xf8 | (depth - 8);
        result[18] = 0xf8 | (depth - 8);
        result[22] = 3;
        for (kind, nal) in (32..=34).zip(nals) {
            result.push(0x80 | kind);
            result.extend_from_slice(&1u16.to_be_bytes());
            result.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            result.extend_from_slice(nal);
        }
        result
    }

    fn append_nal(data: &mut Vec<u8>, nal: &[u8]) {
        data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        data.extend_from_slice(nal);
    }

    #[derive(Default)]
    struct Writer(Vec<bool>);

    impl Writer {
        fn put(&mut self, value: u64, count: u8) {
            for shift in (0..count).rev() {
                self.0.push((value >> shift) & 1 != 0);
            }
        }

        fn ue(&mut self, value: u32) {
            let code = u64::from(value) + 1;
            let width = (64 - code.leading_zeros()) as u8;
            self.put(0, width - 1);
            self.put(code, width);
        }

        fn ptl(&mut self, depth: u8) {
            let (config, _) = fixture();
            for (index, byte) in config[1..13].iter().enumerate() {
                let value = match (depth, index) {
                    (10, 0) => 2,
                    (10, 1) => 0x20,
                    _ => *byte,
                };
                self.put(u64::from(value), 8);
            }
        }

        fn ordering(&mut self, buffers: u8) {
            self.put(1, 1);
            self.ue(u32::from(buffers - 1));
            self.ue(0);
            self.ue(0);
        }

        fn nal(mut self, kind: u8) -> Vec<u8> {
            self.put(1, 1);
            while self.0.len() % 8 != 0 {
                self.put(0, 1);
            }
            let mut nal = vec![kind << 1, 1];
            let mut zeros = 0;
            for bits in self.0.chunks_exact(8) {
                let byte = bits
                    .iter()
                    .fold(0, |byte, bit| (byte << 1) | u8::from(*bit));
                if zeros == 2 && byte <= 3 {
                    nal.push(3);
                    zeros = 0;
                }
                nal.push(byte);
                zeros = if byte == 0 { zeros + 1 } else { 0 };
            }
            nal
        }
    }

    fn vps(depth: u8, buffers: u8) -> Vec<u8> {
        let mut bits = Writer::default();
        bits.put(0, 4);
        bits.put(3, 2);
        bits.put(0, 6);
        bits.put(0, 3);
        bits.put(1, 1);
        bits.put(0xffff, 16);
        bits.ptl(depth);
        bits.ordering(buffers);
        bits.put(0, 6);
        bits.ue(0);
        bits.put(0, 1);
        bits.put(0, 1);
        bits.nal(32)
    }

    fn sps(dimensions: (u32, u32), crop: [u32; 4], depth: u8, buffers: u8) -> Vec<u8> {
        sps_with_color(dimensions, crop, depth, buffers, None)
    }

    #[derive(Clone, Copy)]
    struct VideoSignal {
        full_range: bool,
        description: Option<(u8, u8, u8)>,
    }

    fn sps_with_color(
        dimensions: (u32, u32),
        crop: [u32; 4],
        depth: u8,
        buffers: u8,
        // 外层区分VUI是否存在，内层区分video_signal_type是否存在。
        vui: Option<Option<VideoSignal>>,
    ) -> Vec<u8> {
        let mut bits = Writer::default();
        bits.put(0, 4);
        bits.put(0, 3);
        bits.put(1, 1);
        bits.ptl(depth);
        bits.ue(0);
        bits.ue(1);
        bits.ue(dimensions.0);
        bits.ue(dimensions.1);
        bits.put(u64::from(crop != [0; 4]), 1);
        if crop != [0; 4] {
            for offset in crop {
                bits.ue(offset);
            }
        }
        bits.ue(u32::from(depth - 8));
        bits.ue(u32::from(depth - 8));
        bits.ue(4);
        bits.ordering(buffers);
        for value in [0, 3, 0, 3, 0, 0] {
            bits.ue(value);
        }
        bits.put(0, 4); // scaling、AMP、SAO、PCM。
        bits.ue(0); // short-term RPS数量。
        bits.put(0, 3); // long-term、temporal MVP、smoothing。
        bits.put(u64::from(vui.is_some()), 1);
        if let Some(signal) = vui {
            bits.put(0, 2); // aspect_ratio与overscan均不声明。
            bits.put(u64::from(signal.is_some()), 1);
            if let Some(signal) = signal {
                bits.put(5, 3); // video_format未指定。
                bits.put(u64::from(signal.full_range), 1);
                bits.put(u64::from(signal.description.is_some()), 1);
                if let Some((primaries, transfer, matrix)) = signal.description {
                    for value in [primaries, transfer, matrix] {
                        bits.put(u64::from(value), 8);
                    }
                }
            }
            bits.put(0, 7); // chroma_loc、neutral、field、frame_field、window、timing、restriction。
        }
        bits.put(0, 1); // extension。
        bits.nal(33)
    }

    // 仅合成码流头供结构测试；没有熵编码的像素负载，不视为可解码图片样本。
    fn idr(first_slice: bool, pps_id: u32) -> Vec<u8> {
        let mut bits = Writer::default();
        bits.put(u64::from(first_slice), 1);
        bits.put(0, 1);
        bits.ue(pps_id);
        bits.nal(19)
    }

    #[test]
    fn image_platform_hevc_real_crop_keeps_uncropped_canvas_and_declared_output_separate() {
        let (config, data) = fixture();
        let info = inspect(
            config,
            ByteView::from_slice(data),
            (250, 128),
            LIMIT,
            &token(),
        )
        .unwrap();
        assert_eq!(info.coded_dimensions, (256, 128));
        assert_eq!(info.display_dimensions, (250, 128));
        assert_eq!(info.display_origin, (6, 0));
        assert_eq!(info.bits_per_channel, 8);
        assert_eq!(info.picture_buffers, 1);
        assert_eq!(
            info.color,
            CodedColor {
                model: SampleModel::Ycc,
                cicp: Some(Cicp {
                    primaries: 1,
                    transfer: 13,
                    matrix: 6,
                    full_range: true,
                }),
            }
        );
        assert!(matches!(
            inspect(
                config,
                ByteView::from_slice(data),
                (256, 128),
                LIMIT,
                &token()
            ),
            Err(PlatformError::Invalid { .. })
        ));
        assert!(matches!(
            inspect(
                config,
                ByteView::from_slice(data),
                (250, 128),
                250 * 128 * 4 - 1,
                &token()
            ),
            Err(PlatformError::Budget(_))
        ));
        assert!(inspect(
            config,
            ByteView::from_slice(data),
            (250, 128),
            250 * 128 * 4,
            &token()
        )
        .is_ok());
    }

    #[test]
    fn image_platform_hevc_real_main10_crop_keeps_srgb_color_and_both_origin_offsets() {
        let source = include_bytes!("../tests/data/image_platform/hevc-confwin-420-10bit-all.heic");
        assert_eq!(&source[8..12], b"heix");
        let config = &source[0xac..0x116];
        let data = &source[0x16c..];
        assert_eq!(config[1] & 31, 2); // Main 10。
        assert_eq!(config[16] & 3, 1); // 4:2:0；SPS须由实际预检再次确认。
        let info = inspect(
            config,
            ByteView::from_slice(data),
            (248, 122),
            248 * 122 * 4,
            &token(),
        )
        .unwrap();
        assert_eq!(info.coded_dimensions, (256, 128));
        assert_eq!(info.display_dimensions, (248, 122));
        assert_eq!(info.display_origin, (6, 4));
        assert_eq!(info.bits_per_channel, 10);
        assert_eq!(info.picture_buffers, 1);
        assert_eq!(
            info.color,
            CodedColor {
                model: SampleModel::Ycc,
                cicp: Some(Cicp {
                    primaries: 1,
                    transfer: 13,
                    matrix: 6,
                    full_range: true,
                }),
            }
        );
        assert!(matches!(
            inspect(
                config,
                ByteView::from_slice(data),
                (248, 122),
                248 * 122 * 4 - 1,
                &token(),
            ),
            Err(PlatformError::Budget(_))
        ));
    }

    #[test]
    fn image_platform_hevc_display_origin_comes_from_both_conformance_window_offsets() {
        let nals = [
            vps(10, 1),
            sps((256, 128), [3, 1, 2, 1], 10, 1),
            real_nals()[2].clone(),
        ];
        let config = configuration(&nals, 10);
        let mut data = Vec::new();
        append_nal(&mut data, &idr(true, 0));
        let info = inspect(
            &config,
            ByteView::from_slice(&data),
            (248, 122),
            LIMIT,
            &token(),
        )
        .unwrap();
        assert_eq!(info.coded_dimensions, (256, 128));
        assert_eq!(info.display_dimensions, (248, 122));
        assert_eq!(info.display_origin, (6, 4));
    }

    #[test]
    fn image_platform_hevc_vui_color_preserves_presence_range_and_unspecified_defaults() {
        for (vui, expected) in [
            (None, None),
            (Some(None), None),
            (
                Some(Some(VideoSignal {
                    full_range: false,
                    description: None,
                })),
                Some(Cicp {
                    primaries: 2,
                    transfer: 2,
                    matrix: 2,
                    full_range: false,
                }),
            ),
            (
                Some(Some(VideoSignal {
                    full_range: true,
                    description: None,
                })),
                Some(Cicp {
                    primaries: 2,
                    transfer: 2,
                    matrix: 2,
                    full_range: true,
                }),
            ),
            (
                Some(Some(VideoSignal {
                    full_range: false,
                    description: Some((9, 16, 9)),
                })),
                Some(Cicp {
                    primaries: 9,
                    transfer: 16,
                    matrix: 9,
                    full_range: false,
                }),
            ),
        ] {
            let nals = [
                vps(10, 1),
                sps_with_color((256, 128), [3, 0, 0, 0], 10, 1, vui),
                real_nals()[2].clone(),
            ];
            let config = configuration(&nals, 10);
            let mut data = Vec::new();
            append_nal(&mut data, &idr(true, 0));
            let info = inspect(
                &config,
                ByteView::from_slice(&data),
                (250, 128),
                LIMIT,
                &token(),
            )
            .unwrap();
            assert_eq!(
                info.color,
                CodedColor {
                    model: SampleModel::Ycc,
                    cicp: expected
                }
            );
        }
    }

    #[test]
    fn image_platform_hevc_real_item_can_cross_extent_headers_without_copying() {
        let (config, data) = fixture();
        let extents = [0..1, 1..5, 5..7, 7..data.len()];
        let view = ByteView::from_extents(data, &extents).unwrap();
        let info = inspect(config, view, (250, 128), LIMIT, &token()).unwrap();
        assert_eq!(info.coded_dimensions, (256, 128));
        assert_eq!(info.display_dimensions, (250, 128));
    }

    #[test]
    fn image_platform_hevc_checks_parameter_sets_after_picture_and_binds_the_same_ids() {
        let (config, original) = fixture();
        let nals = real_nals();
        let mut data = original.to_vec();
        for nal in &nals {
            append_nal(&mut data, nal);
        }
        assert!(inspect(
            config,
            ByteView::from_slice(&data),
            (250, 128),
            LIMIT,
            &token()
        )
        .is_ok());
        append_nal(&mut data, &sps((512, 128), [0; 4], 8, 1));
        assert!(matches!(
            inspect(
                config,
                ByteView::from_slice(&data),
                (250, 128),
                LIMIT,
                &token()
            ),
            Err(PlatformError::Unsupported { .. })
        ));
        let mut data = Vec::new();
        append_nal(&mut data, &idr(true, 1));
        assert!(matches!(
            inspect(
                config,
                ByteView::from_slice(&data),
                (250, 128),
                LIMIT,
                &token()
            ),
            Err(PlatformError::Invalid { .. })
        ));
        let mut duplicate = config.to_vec();
        duplicate[22] += 1;
        duplicate.push(0xa1);
        duplicate.extend_from_slice(&1u16.to_be_bytes());
        duplicate.extend_from_slice(&(nals[1].len() as u16).to_be_bytes());
        duplicate.extend_from_slice(&nals[1]);
        assert!(matches!(
            inspect(
                &duplicate,
                ByteView::from_slice(original),
                (250, 128),
                LIMIT,
                &token()
            ),
            Err(PlatformError::Unsupported { .. })
        ));
    }

    #[test]
    fn image_platform_hevc_header_precision_and_dpb_come_from_matching_parameter_sets() {
        for depth in [8, 10] {
            let nals = [
                vps(depth, 4),
                sps((256, 128), [3, 0, 0, 0], depth, 4),
                real_nals()[2].clone(),
            ];
            let config = configuration(&nals, depth);
            let mut data = Vec::new();
            append_nal(&mut data, &idr(true, 0));
            let info = inspect(
                &config,
                ByteView::from_slice(&data),
                (250, 128),
                LIMIT,
                &token(),
            )
            .unwrap();
            assert_eq!(info.bits_per_channel, depth);
            assert_eq!(info.picture_buffers, 4);
            assert_eq!(
                info.color,
                CodedColor {
                    model: SampleModel::Ycc,
                    cicp: None
                }
            );
            let mut config = config;
            config[17] ^= 1;
            assert!(matches!(
                inspect(
                    &config,
                    ByteView::from_slice(&data),
                    (250, 128),
                    LIMIT,
                    &token()
                ),
                Err(PlatformError::Invalid { .. })
            ));
        }
    }

    #[test]
    fn image_platform_hevc_rejects_oversized_canvas_crop_overflow_and_dpb_contradiction() {
        let (_, data) = fixture();
        for (dimensions, crop, declared) in [
            ((65_536, 8), [0; 4], (65_536, 8)),
            ((8192, 4096), [0; 4], (8192, 4096)),
            ((256, 128), [u32::MAX - 1, 2, 0, 0], (250, 128)),
            ((256, 128), [128, 0, 0, 0], (250, 128)),
        ] {
            let nals = [
                vps(8, 1),
                sps(dimensions, crop, 8, 1),
                real_nals()[2].clone(),
            ];
            let config = configuration(&nals, 8);
            assert!(inspect(
                &config,
                ByteView::from_slice(data),
                declared,
                LIMIT,
                &token()
            )
            .is_err());
        }
        let nals = [
            vps(8, 1),
            sps((256, 128), [3, 0, 0, 0], 8, 4),
            real_nals()[2].clone(),
        ];
        assert!(matches!(
            inspect(
                &configuration(&nals, 8),
                ByteView::from_slice(data),
                (250, 128),
                LIMIT,
                &token()
            ),
            Err(PlatformError::Invalid { .. })
        ));
    }

    #[test]
    fn image_platform_hevc_rejects_second_picture_layer_switch_and_truncated_lengths() {
        let (config, original) = fixture();
        let mut data = original.to_vec();
        append_nal(&mut data, &idr(true, 0));
        assert!(matches!(
            inspect(
                config,
                ByteView::from_slice(&data),
                (250, 128),
                LIMIT,
                &token()
            ),
            Err(PlatformError::Unsupported { .. })
        ));
        let mut data = original.to_vec();
        data[4] |= 1;
        assert!(matches!(
            inspect(
                config,
                ByteView::from_slice(&data),
                (250, 128),
                LIMIT,
                &token()
            ),
            Err(PlatformError::Unsupported { .. })
        ));
        for length in [0, 1, 3, original.len() - 1] {
            assert!(inspect(
                config,
                ByteView::from_slice(&original[..length]),
                (250, 128),
                LIMIT,
                &token()
            )
            .is_err());
        }
        let mut data = original.to_vec();
        data[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(inspect(
            config,
            ByteView::from_slice(&data),
            (250, 128),
            LIMIT,
            &token()
        )
        .is_err());
        let mut config = config.to_vec();
        config[21] = (config[21] & !3) | 1;
        assert!(matches!(
            inspect(
                &config,
                ByteView::from_slice(original),
                (250, 128),
                LIMIT,
                &token()
            ),
            Err(PlatformError::Unsupported { .. })
        ));
    }

    #[test]
    fn image_platform_hevc_bounds_parameter_arrays_and_cancellation_before_scan() {
        let (config, data) = fixture();
        let cancel = AtomicBool::new(true);
        assert!(matches!(
            inspect(
                config,
                ByteView::from_slice(data),
                (250, 128),
                LIMIT,
                &cancel
            ),
            Err(PlatformError::Cancelled)
        ));
        let mut config = config[..23].to_vec();
        config[22] = 1;
        config.push(39);
        config.extend_from_slice(&4097u16.to_be_bytes());
        for _ in 0..4097 {
            config.extend_from_slice(&[0, 3, 78, 1, 128]);
        }
        assert!(matches!(
            inspect(
                &config,
                ByteView::from_slice(data),
                (250, 128),
                LIMIT,
                &token()
            ),
            Err(PlatformError::Budget(_))
        ));
    }

    #[test]
    fn image_platform_hevc_rbsp_reader_rejects_bad_prevention_and_unbounded_golomb() {
        for payload in [&[0, 0, 3, 4][..], &[0, 0, 3][..], &[0, 0, 1][..]] {
            let mut nal = vec![66, 1];
            nal.extend_from_slice(payload);
            let cancel = token();
            let mut bits = Bits::new(ByteView::from_slice(&nal), &cancel).unwrap();
            assert!(bits.read(24).is_err());
        }
        let nal = Writer(vec![false; 32]).nal(33);
        let cancel = token();
        assert!(Bits::new(ByteView::from_slice(&nal), &cancel)
            .unwrap()
            .ue(u32::MAX)
            .is_err());
    }

    #[test]
    fn image_platform_hevc_direct_rps_tables_are_bounded_by_count_and_declared_dpb() {
        let mut writer = Writer::default();
        for index in 0..64 {
            if index != 0 {
                writer.put(0, 1);
            }
            writer.ue(0);
            writer.ue(0);
        }
        let nal = writer.nal(33);
        let cancel = token();
        let mut bits = Bits::new(ByteView::from_slice(&nal), &cancel).unwrap();
        short_term_sets(&mut bits, 64, 1).unwrap();
        bits.finish().unwrap();

        let mut writer = Writer::default();
        writer.ue(1);
        writer.ue(1);
        for _ in 0..2 {
            writer.ue(0);
            writer.put(1, 1);
        }
        let nal = writer.nal(33);
        let mut bits = Bits::new(ByteView::from_slice(&nal), &cancel).unwrap();
        short_term_sets(&mut bits, 1, 3).unwrap();
        bits.finish().unwrap();
        let mut bits = Bits::new(ByteView::from_slice(&nal), &cancel).unwrap();
        assert!(short_term_sets(&mut bits, 1, 1).is_err());

        let mut writer = Writer::default();
        writer.ue(0);
        writer.ue(0);
        writer.put(1, 1); // 第二项使用未实现的跨RPS预测。
        let nal = writer.nal(33);
        let mut bits = Bits::new(ByteView::from_slice(&nal), &cancel).unwrap();
        assert!(matches!(
            short_term_sets(&mut bits, 2, 1),
            Err(PlatformError::Unsupported { .. })
        ));
        let mut writer = Writer::default();
        writer.ue(65);
        let nal = writer.nal(33);
        assert!(Bits::new(ByteView::from_slice(&nal), &cancel)
            .unwrap()
            .ue(64)
            .is_err());
    }
}
