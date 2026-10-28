//! 单层reduced still AV1的码流头预检，不执行熵解码或分配像素。
//! 尺寸来自实际Sequence Header及帧头，不以容器ispe代替编码画布。
//! 语法依据：https://aomediacodec.github.io/av1-spec/。
use super::{
    budget, check_cancel, checked_pixels, invalid, unsupported, ByteView, Cicp, CodedColor,
    CodedInfo, PlatformError, SampleModel,
};
use std::sync::atomic::AtomicBool;

const MAX_OBUS: usize = 4096;
const MAX_SEQUENCE_BYTES: usize = 64;

fn malformed() -> PlatformError {
    invalid(
        "AV1码流头或配置无效",
        "Invalid AV1 bitstream header or configuration",
    )
}

fn subset() -> PlatformError {
    unsupported(
        "AV1预检暂仅支持单层、单帧、无超分辨率及胶片颗粒的reduced still图项",
        "AV1 preflight requires a single-layer, single-frame reduced still item without super-resolution or film grain",
    )
}

struct Bits<'a> {
    bytes: ByteView<'a>,
    position: usize,
    cancel: &'a AtomicBool,
}

impl<'a> Bits<'a> {
    fn new(bytes: ByteView<'a>, cancel: &'a AtomicBool) -> Self {
        Self {
            bytes,
            position: 0,
            cancel,
        }
    }

    fn read(&mut self, count: u8) -> Result<u32, PlatformError> {
        check_cancel(self.cancel)?;
        if count > 32 {
            return Err(malformed());
        }
        let end = self
            .position
            .checked_add(usize::from(count))
            .ok_or_else(malformed)?;
        let total = self.bytes.len().checked_mul(8).ok_or_else(malformed)?;
        if end > total {
            return Err(malformed());
        }
        let mut value = 0u32;
        while self.position < end {
            let byte = self.bytes.byte(self.position / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }

    fn flag(&mut self) -> Result<bool, PlatformError> {
        Ok(self.read(1)? != 0)
    }

    fn trailing(&mut self) -> Result<(), PlatformError> {
        if !self.flag()? {
            return Err(malformed());
        }
        let total = self.bytes.len().checked_mul(8).ok_or_else(malformed)?;
        while self.position < total {
            if self.flag()? {
                return Err(malformed());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Sequence {
    dimensions: (u32, u32),
    profile: u8,
    level: u8,
    depth: u8,
    monochrome: bool,
    subsampling_x: bool,
    subsampling_y: bool,
    chroma_position: u8,
    color: CodedColor,
}

fn sequence(
    payload: ByteView<'_>,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Sequence, PlatformError> {
    if payload.len() > MAX_SEQUENCE_BYTES {
        return Err(budget());
    }
    let mut bits = Bits::new(payload, cancel);
    let profile = bits.read(3)? as u8;
    if profile > 2 {
        return Err(malformed());
    }
    let still = bits.flag()?;
    let reduced = bits.flag()?;
    if reduced && !still {
        return Err(malformed());
    }
    if !still || !reduced {
        return Err(subset());
    }
    // reduced分支的tier固定为0，level>7时也没有tier语法位。
    let level = bits.read(5)? as u8;
    let width_bits = bits.read(4)? as u8 + 1;
    let height_bits = bits.read(4)? as u8 + 1;
    let dimensions = (bits.read(width_bits)? + 1, bits.read(height_bits)? + 1);
    checked_pixels(dimensions, max_output_bytes)?;
    bits.read(3)?; // superblock大小、filter intra、intra edge filter。
    if bits.flag()? {
        return Err(subset());
    }
    bits.read(2)?; // CDEF、restoration，不改变编码或显示尺寸。
    let high = bits.flag()?;
    let twelve = profile == 2 && high && bits.flag()?;
    let depth = if twelve {
        12
    } else if high {
        10
    } else {
        8
    };
    let monochrome = profile != 1 && bits.flag()?;
    let description = bits.flag()?;
    let (primaries, transfer, matrix) = if description {
        (bits.read(8)?, bits.read(8)?, bits.read(8)?)
    } else {
        (2, 2, 2)
    };
    let (full_range, subsampling_x, subsampling_y, chroma_position) = if monochrome {
        // 单色没有separate_uv_delta_q语法位。
        (bits.flag()?, true, true, 0)
    } else {
        let (full_range, x, y, position) = if (primaries, transfer, matrix) == (1, 13, 0) {
            // 该分支没有color_range位，语法直接推导为全范围。
            (true, false, false, 0)
        } else {
            let full_range = bits.flag()?;
            let (x, y) = match profile {
                0 => (true, true),
                1 => (false, false),
                _ if depth == 12 => {
                    let x = bits.flag()?;
                    let y = x && bits.flag()?;
                    (x, y)
                }
                _ => (true, false),
            };
            let position = if x && y { bits.read(2)? as u8 } else { 0 };
            if position == 3 {
                return Err(malformed());
            }
            (full_range, x, y, position)
        };
        bits.flag()?; // separate_uv_delta_q。
        (full_range, x, y, position)
    };
    // profile的采样能力与color_config推导结果必须一致。
    if !monochrome
        && (profile == 0 && (!subsampling_x || !subsampling_y)
            || profile == 1 && (subsampling_x || subsampling_y)
            || profile == 2 && depth < 12 && (!subsampling_x || subsampling_y)
            || matrix == 0 && (subsampling_x || subsampling_y))
    {
        return Err(malformed());
    }
    if bits.flag()? {
        return Err(subset());
    }
    bits.trailing()?;
    let model = if monochrome {
        SampleModel::Gray
    } else if matrix == 0 {
        SampleModel::Rgb
    } else if subsampling_x || subsampling_y || matches!(matrix, 1 | 4..=14) {
        SampleModel::Ycc
    } else {
        SampleModel::Unknown
    };
    Ok(Sequence {
        dimensions,
        profile,
        level,
        depth,
        monochrome,
        subsampling_x,
        subsampling_y,
        chroma_position,
        color: CodedColor {
            model,
            // 未显式描述颜色时，2/2/2是码流语法的推导值，不是sRGB声明。
            cicp: Some(Cicp {
                primaries: primaries as u16,
                transfer: transfer as u16,
                matrix: matrix as u16,
                full_range,
            }),
        },
    })
}

fn bind_configuration(config: &[u8], sequence: Sequence) -> Result<(), PlatformError> {
    let expected = (u8::from(sequence.depth > 8) << 6)
        | (u8::from(sequence.depth == 12) << 5)
        | (u8::from(sequence.monochrome) << 4)
        | (u8::from(sequence.subsampling_x) << 3)
        | (u8::from(sequence.subsampling_y) << 2)
        | sequence.chroma_position;
    if config[1] != ((sequence.profile << 5) | sequence.level) || config[2] != expected {
        return Err(malformed());
    }
    Ok(())
}

fn frame(
    payload: ByteView<'_>,
    sequence: Sequence,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<(), PlatformError> {
    let mut bits = Bits::new(payload, cancel);
    bits.flag()?; // disable_cdf_update。
    let screen_content = bits.flag()?;
    if screen_content {
        bits.flag()?; // force_integer_mv；之后Intra固定为1，仍必须消费此位。
    }
    // reduced固定Key/Show/Intra、无frame_id/order_hint/size_override；且已禁superres。
    if bits.flag()? {
        let render = (bits.read(16)? + 1, bits.read(16)? + 1);
        checked_pixels(render, max_output_bytes)?;
        if render != sequence.dimensions {
            return Err(subset());
        }
    }
    Ok(())
}

struct Obus<'a> {
    bytes: ByteView<'a>,
    position: usize,
    count: usize,
    cancel: &'a AtomicBool,
}

impl<'a> Obus<'a> {
    fn next(&mut self) -> Result<Option<(u8, ByteView<'a>)>, PlatformError> {
        check_cancel(self.cancel)?;
        if self.position == self.bytes.len() {
            return Ok(None);
        }
        self.count = self
            .count
            .checked_add(1)
            .filter(|count| *count <= MAX_OBUS)
            .ok_or_else(budget)?;
        let header = self.byte()?;
        if header & 0x81 != 0 {
            return Err(malformed());
        }
        // 本子集OperatingPointIdc=0，不接收任何分层或扩展头。
        if header & 4 != 0 || header & 2 == 0 {
            return Err(subset());
        }
        let kind = (header >> 3) & 15;
        let mut size = 0u64;
        let mut terminated = false;
        for index in 0..8 {
            let byte = self.byte()?;
            size |= u64::from(byte & 0x7f) << (index * 7);
            if byte & 0x80 == 0 {
                terminated = true;
                break;
            }
        }
        if !terminated || size > u64::from(u32::MAX) {
            return Err(malformed());
        }
        let size = usize::try_from(size).map_err(|_| malformed())?;
        let end = self
            .position
            .checked_add(size)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(malformed)?;
        let payload = self.bytes.slice(self.position..end)?;
        self.position = end;
        Ok(Some((kind, payload)))
    }

    fn byte(&mut self) -> Result<u8, PlatformError> {
        check_cancel(self.cancel)?;
        let byte = self.bytes.byte(self.position)?;
        self.position = self.position.checked_add(1).ok_or_else(malformed)?;
        Ok(byte)
    }
}

pub(super) fn inspect(
    config: &[u8],
    data: ByteView<'_>,
    declared: (u32, u32),
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<CodedInfo, PlatformError> {
    check_cancel(cancel)?;
    if config.len() < 4
        || config[0] != 0x81
        || config[3] & 0xe0 != 0
        || config[3] & 0x10 == 0 && config[3] & 0x0f != 0
    {
        return Err(malformed());
    }
    // 首批不接收configOBUs或presentation delay，实际item必须自带唯一序列头。
    if config.len() != 4 || config[3] & 0x10 != 0 {
        return Err(subset());
    }
    checked_pixels(declared, max_output_bytes)?;
    let mut obus = Obus {
        bytes: data,
        position: 0,
        count: 0,
        cancel,
    };
    let mut current = None;
    let mut seen_frame = false;
    let mut seen_delimiter = false;
    while let Some((kind, payload)) = obus.next()? {
        match kind {
            1 => {
                if current.is_some() || seen_frame {
                    return Err(subset());
                }
                let sequence = sequence(payload, max_output_bytes, cancel)?;
                bind_configuration(config, sequence)?;
                if sequence.dimensions != declared {
                    return Err(malformed());
                }
                current = Some(sequence);
            }
            2 => {
                if payload.len() != 0 || seen_delimiter || current.is_some() || seen_frame {
                    return Err(subset());
                }
                seen_delimiter = true;
            }
            6 => {
                if seen_frame {
                    return Err(subset());
                }
                let sequence = current.ok_or_else(malformed)?;
                frame(payload, sequence, max_output_bytes, cancel)?;
                seen_frame = true;
            }
            15 => {} // Padding只按有界OBU范围跳过，不解释其字节为其它OBU。
            _ => return Err(subset()),
        }
    }
    let sequence = current.filter(|_| seen_frame).ok_or_else(malformed)?;
    check_cancel(cancel)?;
    Ok(CodedInfo {
        coded_dimensions: sequence.dimensions,
        display_dimensions: sequence.dimensions,
        display_origin: (0, 0),
        bits_per_channel: sequence.depth,
        // reduced still且实际只有一帧；刷新到各参考槽可别名同一picture。
        // 此处计不同picture，不把参考槽数量当作系统私有分配上限。
        picture_buffers: 1,
        color: sequence.color,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Writer {
        bytes: Vec<u8>,
        position: usize,
    }

    impl Writer {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                position: 0,
            }
        }
        fn write(&mut self, value: u32, count: u8) {
            for bit in (0..count).rev() {
                if self.position % 8 == 0 {
                    self.bytes.push(0);
                }
                let last = self.bytes.last_mut().unwrap();
                *last |= (((value >> bit) & 1) as u8) << (7 - self.position % 8);
                self.position += 1;
            }
        }
        fn trailing(mut self) -> Vec<u8> {
            self.write(1, 1);
            while self.position % 8 != 0 {
                self.write(0, 1);
            }
            self.bytes
        }
    }

    // 自造帧仅包含预检控制前缀，不能用来证明像素或熵解码正确。
    fn header(
        dimensions: (u32, u32),
        profile: u8,
        depth: u8,
        monochrome: bool,
        superres: bool,
        grain: bool,
    ) -> ([u8; 4], Vec<u8>) {
        header_with_color(
            dimensions, profile, depth, monochrome, superres, grain, None, true,
        )
    }

    fn header_with_color(
        dimensions: (u32, u32),
        profile: u8,
        depth: u8,
        monochrome: bool,
        superres: bool,
        grain: bool,
        description: Option<(u8, u8, u8)>,
        full_range: bool,
    ) -> ([u8; 4], Vec<u8>) {
        let mut bits = Writer::new();
        bits.write(u32::from(profile), 3);
        bits.write(1, 1); // still。
        bits.write(1, 1); // reduced。
        bits.write(0, 5); // level，reduced不含tier。
        bits.write(15, 4);
        bits.write(15, 4);
        bits.write(dimensions.0 - 1, 16);
        bits.write(dimensions.1 - 1, 16);
        bits.write(0b011, 3); // 64×64 superblock与两个intra工具。
        bits.write(u32::from(superres), 1);
        bits.write(0, 2); // CDEF/restoration。
        bits.write(u32::from(depth > 8), 1);
        if profile == 2 && depth > 8 {
            bits.write(u32::from(depth == 12), 1);
        }
        if profile != 1 {
            bits.write(u32::from(monochrome), 1);
        }
        bits.write(u32::from(description.is_some()), 1);
        if let Some((primaries, transfer, matrix)) = description {
            for value in [primaries, transfer, matrix] {
                bits.write(u32::from(value), 8);
            }
        }
        let (x, y) = if monochrome {
            bits.write(u32::from(full_range), 1);
            (true, true)
        } else {
            let (x, y) = if description == Some((1, 13, 0)) {
                (false, false)
            } else {
                bits.write(u32::from(full_range), 1);
                let (x, y) = match profile {
                    0 => (true, true),
                    1 => (false, false),
                    _ if depth == 12 => {
                        bits.write(1, 1);
                        bits.write(1, 1);
                        (true, true)
                    }
                    _ => (true, false),
                };
                if x && y {
                    bits.write(0, 2);
                }
                (x, y)
            };
            bits.write(0, 1); // separate_uv_delta_q。
            (x, y)
        };
        bits.write(u32::from(grain), 1);
        let config = [
            0x81,
            profile << 5,
            (u8::from(depth > 8) << 6)
                | (u8::from(depth == 12) << 5)
                | (u8::from(monochrome) << 4)
                | (u8::from(x) << 3)
                | (u8::from(y) << 2),
            0,
        ];
        (config, bits.trailing())
    }

    fn frame_prefix(render: Option<(u32, u32)>, screen: bool) -> Vec<u8> {
        let mut bits = Writer::new();
        bits.write(0, 1);
        bits.write(u32::from(screen), 1);
        if screen {
            bits.write(1, 1);
        }
        bits.write(u32::from(render.is_some()), 1);
        if let Some((width, height)) = render {
            bits.write(width - 1, 16);
            bits.write(height - 1, 16);
        }
        bits.bytes
    }

    fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![(kind << 3) | 2];
        let mut size = payload.len();
        loop {
            let byte = (size & 0x7f) as u8;
            size >>= 7;
            bytes.push(byte | if size == 0 { 0 } else { 0x80 });
            if size == 0 {
                break;
            }
        }
        bytes.extend_from_slice(payload);
        bytes
    }

    fn item(sequence: &[u8], frame: &[u8]) -> Vec<u8> {
        let mut bytes = obu(2, &[]);
        bytes.extend_from_slice(&obu(1, sequence));
        bytes.extend_from_slice(&obu(6, frame));
        bytes
    }

    fn run(
        config: &[u8],
        data: &[u8],
        declared: (u32, u32),
        limit: u64,
    ) -> Result<CodedInfo, PlatformError> {
        inspect(
            config,
            ByteView::from_slice(data),
            declared,
            limit,
            &AtomicBool::new(false),
        )
    }

    #[test]
    fn image_platform_av1_reads_real_white_control_headers_across_extents_without_decoding() {
        let sample = include_bytes!("../tests/data/image_platform/white_1x1.avif");
        let start = sample
            .windows(4)
            .position(|bytes| bytes == b"mdat")
            .unwrap()
            + 4;
        let data = &sample[start..];
        let config = [0x81, 0x20, 0, 0];
        let info = run(&config, data, (1, 1), 4).unwrap();
        assert_eq!(info.coded_dimensions, (1, 1));
        assert_eq!(info.display_dimensions, (1, 1));
        assert_eq!(info.display_origin, (0, 0));
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
        let ranges = [0..1, 1..6, 6..12, 12..data.len()];
        let fragmented = ByteView::from_extents(data, &ranges).unwrap();
        assert_eq!(
            inspect(&config, fragmented, (1, 1), 4, &AtomicBool::new(false)).unwrap(),
            info
        );
    }

    #[test]
    fn image_platform_av1_default_color_keeps_unspecified_values_and_actual_range() {
        for (profile, mono, full_range, model) in [
            (0, false, false, SampleModel::Ycc),
            (0, true, false, SampleModel::Gray),
            (0, true, true, SampleModel::Gray),
            (1, false, true, SampleModel::Unknown),
        ] {
            let (config, sequence) =
                header_with_color((3, 2), profile, 10, mono, false, false, None, full_range);
            let data = item(&sequence, &frame_prefix(None, false));
            let info = run(&config, &data, (3, 2), 24).unwrap();
            assert_eq!(
                info.color,
                CodedColor {
                    model,
                    cicp: Some(Cicp {
                        primaries: 2,
                        transfer: 2,
                        matrix: 2,
                        full_range,
                    }),
                }
            );
        }
    }

    #[test]
    fn image_platform_av1_explicit_matrix_separates_rgb_ycc_reserved_and_gray() {
        for (description, mono, full_range, model) in [
            // 身份分支不编码range位，结果必须是语法隐含的全范围。
            ((1, 13, 0), false, false, SampleModel::Rgb),
            ((9, 16, 0), false, false, SampleModel::Rgb),
            ((1, 13, 6), false, false, SampleModel::Ycc),
            ((9, 16, 9), false, true, SampleModel::Ycc),
            ((2, 2, 3), false, true, SampleModel::Unknown),
            ((2, 2, 255), false, false, SampleModel::Unknown),
            ((9, 16, 9), true, false, SampleModel::Gray),
        ] {
            let profile = if mono { 0 } else { 1 };
            let (config, sequence) = header_with_color(
                (3, 2),
                profile,
                10,
                mono,
                false,
                false,
                Some(description),
                full_range,
            );
            let data = item(&sequence, &frame_prefix(None, false));
            let info = run(&config, &data, (3, 2), 24).unwrap();
            let (primaries, transfer, matrix) = description;
            assert_eq!(
                info.color,
                CodedColor {
                    model,
                    cicp: Some(Cicp {
                        primaries: u16::from(primaries),
                        transfer: u16::from(transfer),
                        matrix: u16::from(matrix),
                        full_range: full_range || !mono && description == (1, 13, 0),
                    }),
                }
            );
        }
    }

    #[test]
    fn image_platform_av1_binds_depth_monochrome_sampling_and_configuration() {
        for (profile, depth, mono) in [
            (0, 8, false),
            (0, 10, true),
            (1, 10, false),
            (2, 8, false),
            (2, 12, false),
        ] {
            let (config, sequence) = header((3, 2), profile, depth, mono, false, false);
            let data = item(&sequence, &frame_prefix(None, true));
            assert_eq!(
                run(&config, &data, (3, 2), 24).unwrap().bits_per_channel,
                depth
            );
            for (index, mask) in [
                (1, 0x20),
                (1, 1),
                (2, 0x80),
                (2, 0x40),
                (2, 0x20),
                (2, 0x10),
                (2, 8),
                (2, 4),
                (2, 1),
            ] {
                let mut bad = config;
                bad[index] ^= mask;
                assert!(matches!(
                    run(&bad, &data, (3, 2), 24),
                    Err(PlatformError::Invalid { .. })
                ));
            }
        }
    }

    #[test]
    fn image_platform_av1_checks_actual_canvas_and_render_before_native_output() {
        let (config, sequence) = header((8000, 4000), 0, 8, false, false, false);
        let data = item(&sequence, &frame_prefix(None, false));
        assert!(run(&config, &data, (8000, 4000), 128_000_000).is_ok());
        assert!(matches!(
            run(&config, &data, (8000, 4000), 127_999_999),
            Err(PlatformError::Budget(_))
        ));
        for dimensions in [(8001, 4000), (65_536, 1)] {
            let (config, sequence) = header(dimensions, 0, 8, false, false, false);
            let data = item(&sequence, &frame_prefix(None, false));
            assert!(matches!(
                run(&config, &data, (1, 1), 256 * 1024 * 1024),
                Err(PlatformError::Budget(_))
            ));
        }
        let (config, sequence) = header((3, 2), 0, 8, false, false, false);
        assert!(matches!(
            run(
                &config,
                &item(&sequence, &frame_prefix(None, false)),
                (2, 3),
                24
            ),
            Err(PlatformError::Invalid { .. })
        ));
        let resized = item(&sequence, &frame_prefix(Some((2, 3)), false));
        assert!(matches!(
            run(&config, &resized, (3, 2), 24),
            Err(PlatformError::Unsupported { .. })
        ));
        let oversized = item(&sequence, &frame_prefix(Some((65_536, 1)), false));
        assert!(matches!(
            run(&config, &oversized, (3, 2), 256 * 1024 * 1024),
            Err(PlatformError::Budget(_))
        ));
    }

    #[test]
    fn image_platform_av1_rejects_unproven_sequence_tools_and_configuration_obus() {
        for (superres, grain) in [(true, false), (false, true)] {
            let (config, sequence) = header((3, 2), 0, 8, false, superres, grain);
            assert!(matches!(
                run(&config, &item(&sequence, &[0]), (3, 2), 24),
                Err(PlatformError::Unsupported { .. })
            ));
        }
        let (config, sequence) = header((3, 2), 0, 8, false, false, false);
        let data = item(&sequence, &[0]);
        let mut ordinary = sequence.clone();
        ordinary[0] &= !0x08; // still=1、reduced=0，合法但不在首批子集。
        assert!(matches!(
            run(&config, &item(&ordinary, &[0]), (3, 2), 24),
            Err(PlatformError::Unsupported { .. })
        ));
        let mut not_still = sequence.clone();
        not_still[0] &= !0x10;
        assert!(matches!(
            run(&config, &item(&not_still, &[0]), (3, 2), 24),
            Err(PlatformError::Invalid { .. })
        ));
        let mut configured = config.to_vec();
        configured.extend_from_slice(&obu(1, &sequence));
        assert!(matches!(
            run(&configured, &data, (3, 2), 24),
            Err(PlatformError::Unsupported { .. })
        ));
        let mut delay = config;
        delay[3] = 0x10;
        assert!(matches!(
            run(&delay, &data, (3, 2), 24),
            Err(PlatformError::Unsupported { .. })
        ));
    }

    #[test]
    fn image_platform_av1_walks_to_eof_and_rejects_late_sequences_frames_and_layers() {
        let (config, sequence) = header((3, 2), 0, 8, false, false, false);
        let original = item(&sequence, &[0]);
        for extra in [
            obu(1, &sequence),
            obu(6, &[0]),
            obu(3, &[0]),
            obu(4, &[0]),
            obu(5, &[0]),
            obu(8, &[0]),
            obu(9, &[]),
            vec![0x36, 0, 1, 0],
        ] {
            let mut data = original.clone();
            data.extend_from_slice(&extra);
            assert!(matches!(
                run(&config, &data, (3, 2), 24),
                Err(PlatformError::Unsupported { .. })
            ));
        }
        let mut padded = original;
        padded.extend_from_slice(&obu(15, &[0x80, 0]));
        assert!(run(&config, &padded, (3, 2), 24).is_ok());
        assert!(matches!(
            run(&config, &obu(6, &[0]), (3, 2), 24),
            Err(PlatformError::Invalid { .. })
        ));
    }

    #[test]
    fn image_platform_av1_checks_obu_sizes_leb128_and_sequence_trailing_bits() {
        let (config, sequence) = header((3, 2), 0, 8, false, false, false);
        let original = item(&sequence, &[0]);
        for length in 0..original.len() {
            assert!(run(&config, &original[..length], (3, 2), 24).is_err());
        }
        for bytes in [
            vec![0x0b, 0],
            vec![0x8a, 0],
            vec![0x0a, 0x80, 0x80, 0x80, 0x80, 0x10],
            vec![0x0a, 0xff, 0xff, 0xff, 0xff, 0x0f],
            vec![0x0a, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80],
        ] {
            assert!(matches!(
                run(&config, &bytes, (3, 2), 24),
                Err(PlatformError::Invalid { .. })
            ));
        }
        assert!(matches!(
            run(&config, &[0x08], (3, 2), 24),
            Err(PlatformError::Unsupported { .. })
        ));
        let mut noncanonical = vec![0x12, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0];
        noncanonical.extend_from_slice(&original[2..]);
        assert!(run(&config, &noncanonical, (3, 2), 24).is_ok());
        let mut bad = sequence;
        let last = bad.last_mut().unwrap();
        *last = 0;
        assert!(matches!(
            run(&config, &item(&bad, &[0]), (3, 2), 24),
            Err(PlatformError::Invalid { .. })
        ));
    }

    #[test]
    fn image_platform_av1_observes_header_limits_and_cancellation() {
        let (config, sequence) = header((3, 2), 0, 8, false, false, false);
        let mut bytes = Vec::new();
        for _ in 0..MAX_OBUS {
            bytes.extend_from_slice(&obu(15, &[]));
        }
        bytes.extend_from_slice(&item(&sequence, &[0]));
        assert!(matches!(
            run(&config, &bytes, (3, 2), 24),
            Err(PlatformError::Budget(_))
        ));
        assert!(matches!(
            run(
                &config,
                &item(&vec![0; MAX_SEQUENCE_BYTES + 1], &[0]),
                (3, 2),
                24
            ),
            Err(PlatformError::Budget(_))
        ));
        assert_eq!(
            inspect(
                &config,
                ByteView::from_slice(&[]),
                (3, 2),
                24,
                &AtomicBool::new(true)
            ),
            Err(PlatformError::Cancelled)
        );
    }
}
