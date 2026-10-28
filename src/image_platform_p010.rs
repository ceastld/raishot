//! 借用有界P010平面，按编码Y′CbCr矩阵还原全范围R′G′B′；不解传递函数。
//! 正步幅包含行补齐，Y后紧接同stride的交错UV；低6位忽略，10位值取高位。
//! 色度采用每个2×2块的最近样本，不声称实现色度位置滤波或保留4:4:4细节。
//! 矩阵后越界的编码RGB裁剪到[0,1]再量化；P010本身没有alpha，输出始终不透明。
use super::{Cicp, PlatformError};

pub(super) struct Source<'a> {
    bytes: &'a [u8],
    stride: usize,
    uv_start: usize,
    display_origin: (u32, u32),
    display_dimensions: (u32, u32),
    kr: f64,
    kb: f64,
    full_range: bool,
}

fn invalid() -> PlatformError {
    super::invalid(
        "P010步幅、平面长度、裁剪或采样坐标无效",
        "Invalid P010 stride, plane length, crop, or sampling coordinates",
    )
}

impl<'a> Source<'a> {
    /// dimensions是物理平面尺寸；display_origin以其左上角为原点，不包含EXIF方向。
    /// 只接受Y+UV的准确长度；行内padding由stride声明，不接受未声明的尾部数据。
    /// CP/TC由后续HDR计划判断；本层仅消费matrix与full_range，不应用EOTF或色管。
    pub(super) fn new(
        bytes: &'a [u8],
        dimensions: (u32, u32),
        stride: usize,
        display_origin: (u32, u32),
        display_dimensions: (u32, u32),
        cicp: Cicp,
    ) -> Result<Self, PlatformError> {
        let (width, height) = dimensions;
        let (origin_x, origin_y) = display_origin;
        let (display_width, display_height) = display_dimensions;
        if width == 0
            || height == 0
            || width % 2 != 0
            || height % 2 != 0
            || display_width == 0
            || display_height == 0
            || display_width % 2 != 0
            || display_height % 2 != 0
            || origin_x % 2 != 0
            || origin_y % 2 != 0
            || stride == 0
            || stride % 2 != 0
        {
            return Err(invalid());
        }
        super::coded::checked_pixels(dimensions, super::MAX_BYTES)?;
        let row_bytes = usize::try_from(width)
            .ok()
            .and_then(|width| width.checked_mul(2))
            .ok_or_else(super::budget)?;
        if stride < row_bytes
            || origin_x
                .checked_add(display_width)
                .is_none_or(|end| end > width)
            || origin_y
                .checked_add(display_height)
                .is_none_or(|end| end > height)
        {
            return Err(invalid());
        }
        let uv_start = stride
            .checked_mul(usize::try_from(height).map_err(|_| super::budget())?)
            .ok_or_else(super::budget)?;
        let expected = stride
            .checked_mul(usize::try_from(height / 2).map_err(|_| super::budget())?)
            .and_then(|uv_bytes| uv_start.checked_add(uv_bytes))
            .filter(|bytes| *bytes as u64 <= super::MAX_BYTES)
            .ok_or_else(super::budget)?;
        if bytes.len() as u64 > super::MAX_BYTES {
            return Err(super::budget());
        }
        if bytes.len() != expected {
            return Err(invalid());
        }
        let (kr, kb) = match cicp.matrix {
            1 => (0.2126, 0.0722),
            5 | 6 => (0.299, 0.114),
            9 => (0.2627, 0.0593),
            _ => {
                return Err(super::coded::unsupported(
                    "P010采样暂仅支持BT.709、BT.601与BT.2020非恒定亮度矩阵",
                    "P010 sampling supports only BT.709, BT.601, and BT.2020 non-constant-luminance matrices",
                ))
            }
        };
        Ok(Self {
            bytes,
            stride,
            uv_start,
            display_origin,
            display_dimensions,
            kr,
            kb,
            full_range: cicp.full_range,
        })
    }

    fn sample(&self, offset: usize) -> Result<f64, PlatformError> {
        let end = offset.checked_add(2).ok_or_else(invalid)?;
        let word = self.bytes.get(offset..end).ok_or_else(invalid)?;
        Ok(f64::from(u16::from_le_bytes([word[0], word[1]]) >> 6))
    }

    /// x/y是未定向display裁剪内的坐标；调用方按行检查取消并负责唯一的方向应用。
    /// 返回的是编码域UInt16 RGBA值，不是linear、sRGB或再次扩展范围后的YUV。
    pub(super) fn pixel(&self, x: u32, y: u32) -> Result<[u16; 4], PlatformError> {
        if x >= self.display_dimensions.0 || y >= self.display_dimensions.1 {
            return Err(invalid());
        }
        let x = usize::try_from(self.display_origin.0 + x).map_err(|_| invalid())?;
        let y = usize::try_from(self.display_origin.1 + y).map_err(|_| invalid())?;
        // new已核physical/crop/stride/总长，以下乘加均在同一有效平面范围内。
        let luma = self.sample(y * self.stride + x * 2)?;
        let uv = self.uv_start + (y / 2) * self.stride + (x / 2) * 4;
        let cb = self.sample(uv)? - 512.0;
        let cr = self.sample(uv + 2)? - 512.0;
        let (luma, cb, cr) = if self.full_range {
            (luma / 1023.0, cb / 1023.0, cr / 1023.0)
        } else {
            // 10位limited Y=64..940，Cb/Cr=64..960，中心512。
            // 保留footroom/headroom参与矩阵，只在最终RGB处裁剪。
            ((luma - 64.0) / 876.0, cb / 896.0, cr / 896.0)
        };
        let r = luma + 2.0 * (1.0 - self.kr) * cr;
        let b = luma + 2.0 * (1.0 - self.kb) * cb;
        let g = (luma - self.kr * r - self.kb * b) / (1.0 - self.kr - self.kb);
        let quantize = |value: f64| (value.clamp(0.0, 1.0) * 65535.0).round() as u16;
        Ok([quantize(r), quantize(g), quantize(b), u16::MAX])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(matrix: u16, full_range: bool) -> Cicp {
        Cicp {
            primaries: 9,
            transfer: 16,
            matrix,
            full_range,
        }
    }

    fn put(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&(value << 6).to_le_bytes());
    }

    fn block(y: u16, cb: u16, cr: u16) -> Vec<u8> {
        let mut bytes = vec![0; 12];
        for offset in [0, 2, 4, 6] {
            put(&mut bytes, offset, y);
        }
        put(&mut bytes, 8, cb);
        put(&mut bytes, 10, cr);
        bytes
    }

    fn sample(bytes: &[u8], matrix: u16, full_range: bool) -> [u16; 4] {
        Source::new(bytes, (2, 2), 4, (0, 0), (2, 2), signal(matrix, full_range))
            .unwrap()
            .pixel(0, 0)
            .unwrap()
    }

    #[test]
    fn image_platform_p010_black_white_neutral_obey_full_and_limited_ranges() {
        for matrix in [1, 5, 6, 9] {
            for (full_range, black, white, middle, expected) in
                [(true, 0, 1023, 512, 32800), (false, 64, 940, 502, 32768)]
            {
                assert_eq!(
                    sample(&block(black, 512, 512), matrix, full_range),
                    [0, 0, 0, 65535]
                );
                assert_eq!(
                    sample(&block(white, 512, 512), matrix, full_range),
                    [65535; 4]
                );
                assert_eq!(
                    sample(&block(middle, 512, 512), matrix, full_range),
                    [expected, expected, expected, 65535]
                );
            }
        }
        assert_eq!(sample(&block(0, 512, 512), 9, false), [0, 0, 0, 65535]);
        assert_eq!(sample(&block(1023, 512, 512), 9, false), [65535; 4]);
    }

    #[test]
    fn image_platform_p010_literal_chroma_goldens_distinguish_matrix_and_range() {
        // 手工码值按标准矩阵独立计算，断言不是由本采样器的逆变换生成。
        for (matrix, full_range, values, expected) in [
            (1, true, [512, 768, 256], [6973, 37405, 63231, 65535]),
            (5, true, [512, 768, 256], [9807, 38867, 61860, 65535]),
            (6, true, [512, 768, 256], [9807, 38867, 61860, 65535]),
            (9, true, [512, 768, 256], [8616, 39471, 63654, 65535]),
            (1, false, [502, 736, 288], [6966, 37368, 63169, 65535]),
            (5, false, [502, 736, 288], [9797, 38829, 61800, 65535]),
            (6, false, [502, 736, 288], [9797, 38829, 61800, 65535]),
            (9, false, [502, 736, 288], [8608, 39432, 63592, 65535]),
        ] {
            assert_eq!(
                sample(&block(values[0], values[1], values[2]), matrix, full_range),
                expected
            );
        }
    }

    #[test]
    fn image_platform_p010_clips_rgb_after_matrix_without_first_clipping_luma_headroom() {
        // Y低于limited黑位时，非中性色度仍可留下正的R′；不能提前把Y截为0。
        let actual = sample(&block(0, 512, 960), 9, false);
        assert_eq!(actual, [43531, 0, 0, 65535]);
        let opposite = sample(&block(1023, 64, 512), 9, false);
        assert_eq!(opposite[0], 65535);
        assert_eq!(opposite[1], 65535);
        assert!(opposite[2] > 0 && opposite[2] < 20000);
    }

    #[test]
    fn image_platform_p010_nearest_crop_uses_physical_uv_origin_and_padded_non_square_rows() {
        let stride = 20usize;
        let mut bytes = vec![0xa5; stride * 6];
        // 8×4物理Y，接8×2 UV；行尾4字节保持哨兵，不能解释成样本。
        for y in 0..4 {
            for x in 0..8 {
                put(&mut bytes, y * stride + x * 2, 64);
            }
        }
        for y in 0..2 {
            for pair in 0..4 {
                put(&mut bytes, stride * 4 + y * stride + pair * 4, 0);
                put(&mut bytes, stride * 4 + y * stride + pair * 4 + 2, 0);
            }
        }
        for (x, y, value) in [(2, 2, 512), (3, 2, 1023), (2, 3, 0), (3, 3, 256)] {
            put(&mut bytes, y * stride + x * 2, value);
        }
        let uv_row = stride * 5;
        put(&mut bytes, uv_row + 4, 512);
        put(&mut bytes, uv_row + 6, 512);
        put(&mut bytes, uv_row + 8, 768);
        put(&mut bytes, uv_row + 10, 256);
        for y in 2..4 {
            for x in 4..6 {
                put(&mut bytes, y * stride + x * 2, 512);
            }
        }
        let source = Source::new(&bytes, (8, 4), stride, (2, 2), (4, 2), signal(9, true)).unwrap();
        assert_eq!(source.pixel(0, 0).unwrap(), [32800, 32800, 32800, 65535]);
        assert_eq!(source.pixel(1, 0).unwrap(), [65535; 4]);
        assert_eq!(source.pixel(0, 1).unwrap(), [0, 0, 0, 65535]);
        assert_eq!(source.pixel(1, 1).unwrap(), [16400, 16400, 16400, 65535]);
        for (x, y) in [(2, 0), (3, 0), (2, 1), (3, 1)] {
            assert_eq!(source.pixel(x, y).unwrap(), [8616, 39471, 63654, 65535]);
        }
    }

    #[test]
    fn image_platform_p010_little_endian_high_ten_bits_ignore_low_six_bits() {
        let mut bytes = block(512, 768, 256);
        assert_eq!(&bytes[..2], &[0, 128]);
        for word in bytes.chunks_exact_mut(2) {
            word[0] |= 0x3f;
        }
        assert_eq!(sample(&bytes, 9, true), [8616, 39471, 63654, 65535]);
    }

    #[test]
    fn image_platform_p010_color_primaries_and_transfer_are_not_applied_by_sampling() {
        let bytes = block(512, 512, 512);
        for (primaries, transfer) in [(1, 13), (9, 16), (9, 18), (2, 2), (255, 255)] {
            let mut cicp = signal(9, true);
            cicp.primaries = primaries;
            cicp.transfer = transfer;
            let source = Source::new(&bytes, (2, 2), 4, (0, 0), (2, 2), cicp).unwrap();
            assert_eq!(source.pixel(1, 1).unwrap(), [32800, 32800, 32800, 65535]);
        }
        for matrix in [0, 2, 3, 4, 7, 8, 10, 11, 14, 255] {
            assert!(matches!(
                Source::new(&bytes, (2, 2), 4, (0, 0), (2, 2), signal(matrix, true)),
                Err(PlatformError::Unsupported { .. })
            ));
        }
    }

    #[test]
    fn image_platform_p010_rejects_incomplete_planes_odd_geometry_and_crop_overflow() {
        let bytes = block(512, 512, 512);
        for (dimensions, stride, origin, display) in [
            ((0, 2), 4, (0, 0), (2, 2)),
            ((3, 2), 6, (0, 0), (2, 2)),
            ((2, 3), 4, (0, 0), (2, 2)),
            ((2, 2), 0, (0, 0), (2, 2)),
            ((2, 2), 3, (0, 0), (2, 2)),
            ((2, 2), 2, (0, 0), (2, 2)),
            ((2, 2), 4, (1, 0), (2, 2)),
            ((2, 2), 4, (0, 1), (2, 2)),
            ((2, 2), 4, (0, 0), (1, 2)),
            ((2, 2), 4, (0, 0), (2, 0)),
            ((2, 2), 4, (2, 0), (2, 2)),
            ((2, 2), 4, (0, 2), (2, 2)),
            ((2, 2), 4, (u32::MAX - 1, 0), (2, 2)),
            ((2, 2), 4, (0, u32::MAX - 1), (2, 2)),
        ] {
            assert!(matches!(
                Source::new(&bytes, dimensions, stride, origin, display, signal(9, true)),
                Err(PlatformError::Invalid { .. })
            ));
        }
        for length in 0..bytes.len() {
            assert!(matches!(
                Source::new(&bytes[..length], (2, 2), 4, (0, 0), (2, 2), signal(9, true)),
                Err(PlatformError::Invalid { .. })
            ));
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(matches!(
            Source::new(&extra, (2, 2), 4, (0, 0), (2, 2), signal(9, true)),
            Err(PlatformError::Invalid { .. })
        ));
        let source = Source::new(&bytes, (2, 2), 4, (0, 0), (2, 2), signal(9, true)).unwrap();
        for (x, y) in [(2, 0), (0, 2), (u32::MAX, 0), (0, u32::MAX)] {
            assert!(matches!(
                source.pixel(x, y),
                Err(PlatformError::Invalid { .. })
            ));
        }
    }

    #[test]
    fn image_platform_p010_budget_checks_before_access_or_allocation() {
        for (dimensions, stride) in [
            ((65_536, 2), 131_072),
            ((8_000, 4_002), 16_000),
            ((2, 2), usize::MAX - 1),
            ((2, 2), super::super::MAX_BYTES as usize),
        ] {
            assert!(matches!(
                Source::new(&[], dimensions, stride, (0, 0), (2, 2), signal(9, true)),
                Err(PlatformError::Budget(_))
            ));
        }
    }
}
