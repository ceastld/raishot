//! 解码回归专用固定条空数据；不调用任何编码器，预期文本由固定输入确定。
use crate::capture::Image;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FixtureFormat {
    Code128,
    Code39,
    Ean13,
    Ean8,
    Upca,
    Upce,
    Itf,
}

const SCALE: u32 = 2;
const BAR_HEIGHT: u32 = 24;
const QUIET_X: u32 = 24;
const QUIET_Y: u32 = 0;
const GAP: u32 = 16;

pub(super) fn expected_text(format: FixtureFormat) -> &'static str {
    match format {
        FixtureFormat::Code128 => "AB12",
        FixtureFormat::Code39 => "ABC",
        FixtureFormat::Ean13 => "5901234123457",
        FixtureFormat::Ean8 => "96385074",
        FixtureFormat::Upca => "036000291452",
        FixtureFormat::Upce => "05096893",
        FixtureFormat::Itf => "123456",
    }
}

/// 返回无文字标签的白底不透明 BGRA 图与独立预期文本；条宽单位为 2 像素。
/// 黑条区域从 (24,0) 开始，整图高 24；左右各留 24 白像素。
pub(super) fn barcode(format: FixtureFormat) -> (Image, &'static str) {
    let image = match format {
        // Code B：start 104；A/B/1/2 为 33/34/17/18。
        // (104 + 33 + 34*2 + 17*3 + 18*4) % 103 = 19，最后为 stop 106。
        FixtureFormat::Code128 => raster_runs(concat!(
            "211214", "111323", "131123", "123221", "223211", "221132", "2331112"
        )),
        // * A B C *，窄/宽比 1:2，字符间一单位白条；不附加可选 Mod 43 校验。
        FixtureFormat::Code39 => raster_runs(concat!(
            "121121211",
            "1",
            "211112112",
            "1",
            "112112112",
            "1",
            "212112111",
            "1",
            "121121211"
        )),
        // 5 的左半奇偶排列为 L G G L L G；加权和 83，末位校验 7。
        FixtureFormat::Ean13 => ean13_with_last("1000100"),
        // 左 9638、右 5074；加权和 86，末位校验 4。
        FixtureFormat::Ean8 => raster_bits(concat!(
            "101", "0001011", "0101111", "0111101", "0110111", "01010", "1001110", "1110010",
            "1000100", "1011100", "101"
        )),
        // 左 036000、右 291452；加权和 58，末位校验 2。
        FixtureFormat::Upca => raster_bits(concat!(
            "101", "0001101", "0111101", "0101111", "0001101", "0001101", "0001101", "01010",
            "1101100", "1110100", "1100110", "1011100", "1001110", "1101100", "101"
        )),
        // 数字 509689 使用 G G L L L G；展开为 050968000093，加权和 67，校验 3。
        FixtureFormat::Upce => raster_bits(concat!(
            "101", "0111001", "0100111", "0001011", "0101111", "0110111", "0010111", "010101"
        )),
        // 交错数字对 12 / 34 / 56，窄/宽比 1:3；本码制不强制附加校验位。
        FixtureFormat::Itf => raster_runs(concat!(
            "1111",
            "3113111133",
            "3131131113",
            "3113331111",
            "311"
        )),
    };
    (image, expected_text(format))
}

fn ean13_with_last(last: &str) -> Image {
    // 完整前 12 位始终为 590123412345；最后一个 R 模式单独替换供坏校验样本使用。
    const PREFIX: &str = concat!(
        "101", "0001011", "0100111", "0110011", "0010011", "0111101", "0011101", "01010",
        "1100110", "1101100", "1000010", "1011100", "1001110"
    );
    raster_bits(&format!("{PREFIX}{last}101"))
}

/// 5901234123458：只将末位从合法的 7 改为 8，条形、保护条和留白均完整。
pub(super) fn invalid_checksum_fixture() -> Image {
    ean13_with_last("1001000")
}

/// 固定十进制宽度串从黑条开始，逐段黑白交替；只展开条空，不解释符号或计算校验。
fn raster_runs(runs: &str) -> Image {
    let mut bits = String::new();
    let mut black = true;
    for run in runs.bytes() {
        assert!((b'1'..=b'4').contains(&run));
        for _ in 0..run - b'0' {
            bits.push(if black { '1' } else { '0' });
        }
        black = !black;
    }
    raster_bits(&bits)
}

fn raster_bits(bits: &str) -> Image {
    let width = bits.len() as u32 * SCALE + QUIET_X * 2;
    let mut image = white(width, BAR_HEIGHT + QUIET_Y * 2);
    for (module, bit) in bits.bytes().enumerate() {
        assert!(bit == b'0' || bit == b'1');
        if bit == b'0' {
            continue;
        }
        let left = QUIET_X + module as u32 * SCALE;
        for y in QUIET_Y..QUIET_Y + BAR_HEIGHT {
            for x in left..left + SCALE {
                let offset = (y as usize * width as usize + x as usize) * 4;
                image.bgra[offset..offset + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
    image
}

fn white(width: u32, height: u32) -> Image {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .expect("small fixture dimensions");
    let bytes = usize::try_from(bytes).expect("fixture dimensions fit memory indexing");
    assert!(bytes <= isize::MAX as usize);
    Image {
        width,
        height,
        bgra: vec![255; bytes],
        readback_ms: 0.,
    }
}

fn blit(source: &Image, target: &mut Image, left: u32, top: u32) {
    assert!(u64::from(left) + u64::from(source.width) <= u64::from(target.width));
    assert!(u64::from(top) + u64::from(source.height) <= u64::from(target.height));
    let stride = source.width as usize * 4;
    assert_eq!(source.bgra.len(), stride * source.height as usize);
    for y in 0..source.height as usize {
        let destination = ((top as usize + y) * target.width as usize + left as usize) * 4;
        target.bgra[destination..destination + stride]
            .copy_from_slice(&source.bgra[y * stride..(y + 1) * stride]);
    }
}

/// 第一幅图在左，第二幅图的 x 为 first.width + 16；垂直居中，原有留白保留。
pub(super) fn compose_side_by_side(first: &Image, second: &Image) -> Image {
    let width = first
        .width
        .checked_add(GAP)
        .and_then(|v| v.checked_add(second.width))
        .expect("small fixture widths");
    let height = first.height.max(second.height);
    let mut image = white(width, height);
    blit(first, &mut image, 0, (height - first.height) / 2);
    blit(
        second,
        &mut image,
        first.width + GAP,
        (height - second.height) / 2,
    );
    image
}

/// 第一幅图在上，第二幅图的 y 为 first.height + 16；水平居中，原有留白保留。
pub(super) fn stacked(first: &Image, second: &Image) -> Image {
    let width = first.width.max(second.width);
    let height = first
        .height
        .checked_add(GAP)
        .and_then(|v| v.checked_add(second.height))
        .expect("small fixture heights");
    let mut image = white(width, height);
    blit(first, &mut image, (width - first.width) / 2, 0);
    blit(
        second,
        &mut image,
        (width - second.width) / 2,
        first.height + GAP,
    );
    image
}

/// 顺时针 90 度，逐像素重排同一小幅测试光栅，保留 alpha 与原有留白。
pub(super) fn rotate90(source: &Image) -> Image {
    let mut image = white(source.height, source.width);
    assert_eq!(image.bgra.len(), source.bgra.len());
    for y in 0..source.height {
        for x in 0..source.width {
            let origin = (y as usize * source.width as usize + x as usize) * 4;
            let destination =
                (x as usize * image.width as usize + (source.height - 1 - y) as usize) * 4;
            image.bgra[destination..destination + 4]
                .copy_from_slice(&source.bgra[origin..origin + 4]);
        }
    }
    image
}
