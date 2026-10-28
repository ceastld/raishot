//! 与自有 DOM canvas 的公开世界像素合同独立核对，不读取产品预览作期望。
use crate::{capture::Image, Result};
use serde::Serialize;

#[derive(Clone, Copy, Debug)]
pub(super) struct Spec {
    pub dpr: f64,
    pub width: u32,
    pub viewport_height: u32,
    pub document_height: u32,
}
impl Spec {
    pub fn new(dpr: f64) -> Result<Self> {
        if !dpr.is_finite() || dpr <= 0. {
            return Err("浏览器 DPR 无效".into());
        }
        let integer = |css: f64| -> Result<u32> {
            let value = css * dpr;
            if (value - value.round()).abs() > 0.000_001 || !(1. ..=2048.).contains(&value) {
                return Err("首组 CSS×DPR 必须为不超过2048的整数物理像素".into());
            }
            Ok(value.round() as u32)
        };
        Ok(Self {
            dpr,
            width: integer(320.)?,
            viewport_height: integer(240.)?,
            document_height: integer(720.)?,
        })
    }
    fn edge(self, css: u32) -> u32 {
        (f64::from(css) * self.dpr).round() as u32
    }
    pub fn pixel(self, x: u32, y: u32) -> [u8; 4] {
        for band in 0..3 {
            let top = self.edge(band * 240);
            let bottom = self.edge(band * 240 + 240);
            if y >= top && y < bottom {
                let left_end = self.edge(12);
                let right_start = self.edge(308);
                let top_end = self.edge(band * 240 + 12);
                let bottom_start = self.edge(band * 240 + 228);
                // 四角依随世界文档滚动；没有 fixed/sticky 层。
                let rgba = if x < left_end && y < top_end {
                    Some([251, 19, 73, 255])
                } else if x >= right_start && y < top_end {
                    Some([17, 239, 101, 255])
                } else if x < left_end && y >= bottom_start {
                    Some([37, 89, 247, 255])
                } else if x >= right_start && y >= bottom_start {
                    Some([233, 181, 29, 255])
                } else if x >= left_end && x < right_start && y < self.edge(band * 240 + 2) {
                    Some([
                        (90 + band * 31) as u8,
                        (40 + band * 47) as u8,
                        (170 - band * 29) as u8,
                        255,
                    ])
                } else {
                    None
                };
                if let Some([r, g, b, a]) = rgba {
                    return [b, g, r, a];
                }
                break;
            }
        }
        let n = y.wrapping_mul(157).wrapping_add(x.wrapping_mul(71))
            ^ y.wrapping_mul(17).wrapping_add(x.wrapping_mul(113));
        [
            (n % 251) as u8,
            (y.wrapping_mul(13).wrapping_add(x.wrapping_mul(19)) % 251) as u8,
            (y.wrapping_mul(y).wrapping_add(x.wrapping_mul(41)) % 251) as u8,
            255,
        ]
    }
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct PixelCheck {
    pub pixels: u64,
    pub channels: u64,
    pub max_channel_difference: u8,
    pub tolerance: u8,
}
fn validate(image: &Image) -> Result<()> {
    if image.width == 0
        || image.height == 0
        || u64::from(image.width) * u64::from(image.height) > 8_000_000
        || image.bgra.len() as u64 != u64::from(image.width) * u64::from(image.height) * 4
    {
        return Err("浏览器 oracle 图像尺寸或缓冲无效".into());
    }
    Ok(())
}
fn check_pixel(
    actual: &[u8],
    expected: [u8; 4],
    x: u32,
    y: u32,
    tolerance: u8,
    maximum: &mut u8,
) -> Result<()> {
    if actual[3] != 255 || expected[3] != 255 {
        return Err("浏览器首组内容必须完全不透明".into());
    }
    for channel in 0..4 {
        let difference = actual[channel].abs_diff(expected[channel]);
        *maximum = (*maximum).max(difference);
        if difference > tolerance {
            return Err(format!("浏览器像素不符：({x},{y}) channel={channel}, actual={actual:?}, expected={expected:?}, diff={difference}, limit={tolerance}").into());
        }
    }
    Ok(())
}
pub(super) fn world(
    image: &Image,
    spec: Spec,
    first_row: u32,
    tolerance: u8,
) -> Result<PixelCheck> {
    validate(image)?;
    if image.width != spec.width
        || first_row
            .checked_add(image.height)
            .is_none_or(|end| end > spec.document_height)
    {
        return Err("浏览器世界坐标 oracle 范围无效".into());
    }
    let mut maximum = 0;
    for y in 0..image.height {
        for x in 0..image.width {
            let index = (u64::from(y) * u64::from(image.width) + u64::from(x)) * 4;
            check_pixel(
                &image.bgra[index as usize..index as usize + 4],
                spec.pixel(x, first_row + y),
                x,
                first_row + y,
                tolerance,
                &mut maximum,
            )?;
        }
    }
    let pixels = u64::from(image.width) * u64::from(image.height);
    Ok(PixelCheck {
        pixels,
        channels: pixels * 4,
        max_channel_difference: maximum,
        tolerance,
    })
}
pub(super) fn region(
    actual: &Image,
    expected: &Image,
    offset: (u32, u32),
    tolerance: u8,
) -> Result<PixelCheck> {
    validate(actual)?;
    validate(expected)?;
    if offset
        .0
        .checked_add(expected.width)
        .is_none_or(|right| right > actual.width)
        || offset
            .1
            .checked_add(expected.height)
            .is_none_or(|bottom| bottom > actual.height)
    {
        return Err("renderer 对照范围超出自有 client".into());
    }
    let mut maximum = 0;
    for y in 0..expected.height {
        for x in 0..expected.width {
            let a = ((y + offset.1) as usize * actual.width as usize + (x + offset.0) as usize) * 4;
            let e = (y as usize * expected.width as usize + x as usize) * 4;
            let p = &expected.bgra[e..e + 4];
            check_pixel(
                &actual.bgra[a..a + 4],
                [p[0], p[1], p[2], p[3]],
                x,
                y,
                tolerance,
                &mut maximum,
            )?;
        }
    }
    let pixels = u64::from(expected.width) * u64::from(expected.height);
    Ok(PixelCheck {
        pixels,
        channels: pixels * 4,
        max_channel_difference: maximum,
        tolerance,
    })
}
#[derive(Clone, Debug, Serialize)]
pub(super) struct Calibration {
    pub offset_left: u32,
    pub offset_top: u32,
    pub unique_matches: usize,
    pub checked: PixelCheck,
}
pub(super) fn calibrate(
    client: &Image,
    renderer: &Image,
    spec: Spec,
    tolerance: u8,
) -> Result<Calibration> {
    validate(client)?;
    validate(renderer)?;
    if renderer.width != spec.width
        || renderer.height != spec.viewport_height
        || renderer.width > client.width
        || renderer.height > client.height
    {
        return Err("renderer 与自有 client/DPR 尺寸无法校准".into());
    }
    let inset = spec.edge(6).max(1);
    let anchors = [
        (inset, inset),
        (renderer.width - inset - 1, inset),
        (inset, renderer.height - inset - 1),
        (renderer.width - inset - 1, renderer.height - inset - 1),
        (renderer.width / 2, renderer.height / 2),
    ];
    let mut matches = Vec::new();
    for top in 0..=client.height - renderer.height {
        for left in 0..=client.width - renderer.width {
            let plausible = anchors.iter().all(|&(x, y)| {
                let a = ((top + y) as usize * client.width as usize + (left + x) as usize) * 4;
                let e = (y as usize * renderer.width as usize + x as usize) * 4;
                client.bgra[a + 3] == 255
                    && renderer.bgra[e + 3] == 255
                    && (0..3)
                        .all(|c| client.bgra[a + c].abs_diff(renderer.bgra[e + c]) <= tolerance)
            });
            if plausible {
                if let Ok(checked) = region(client, renderer, (left, top), tolerance) {
                    matches.push(Calibration {
                        offset_left: left,
                        offset_top: top,
                        unique_matches: 1,
                        checked,
                    });
                    if matches.len() > 1 {
                        return Err("renderer 在自有 client 中有多个完整匹配，拒绝猜测偏移".into());
                    }
                }
            }
        }
    }
    matches
        .pop()
        .ok_or_else(|| "renderer 在自有 client 中没有同相位完整匹配".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn image(spec: Spec) -> Image {
        let mut bgra = Vec::new();
        for y in 0..spec.viewport_height {
            for x in 0..spec.width {
                bgra.extend_from_slice(&spec.pixel(x, y));
            }
        }
        Image {
            width: spec.width,
            height: spec.viewport_height,
            bgra,
            readback_ms: 0.,
        }
    }
    fn paste(client: &mut Image, content: &Image, left: u32, top: u32) {
        for y in 0..content.height {
            let dst = ((top + y) as usize * client.width as usize + left as usize) * 4;
            let src = y as usize * content.width as usize * 4;
            client.bgra[dst..dst + content.width as usize * 4]
                .copy_from_slice(&content.bgra[src..src + content.width as usize * 4]);
        }
    }
    #[test]
    fn long_browser_oracle_unique_physical_phase_rejects_ambiguous_client_and_bad_dpr() {
        let spec = Spec::new(1.25).unwrap();
        let renderer = image(spec);
        let mut client = Image {
            width: 900,
            height: 650,
            bgra: [0, 0, 0, 255].repeat(900 * 650),
            readback_ms: 0.,
        };
        paste(&mut client, &renderer, 17, 39);
        let found = calibrate(&client, &renderer, spec, 1).unwrap();
        assert_eq!((found.offset_left, found.offset_top), (17, 39));
        assert_eq!(found.checked.pixels, 400 * 300);
        paste(&mut client, &renderer, 480, 340);
        assert!(calibrate(&client, &renderer, spec, 1).is_err());
        assert!(Spec::new(f64::NAN).is_err());
        assert!(Spec::new(1.333).is_err());
    }
    #[test]
    fn long_browser_oracle_world_noise_and_markers_detect_tail_and_alpha_errors() {
        let spec = Spec::new(1.).unwrap();
        let mut actual = image(spec);
        assert_eq!(world(&actual, spec, 0, 1).unwrap().pixels, 320 * 240);
        let last = actual.bgra.len() - 4;
        actual.bgra[last] ^= 127;
        assert!(world(&actual, spec, 0, 1).is_err());
        actual.bgra[last..last + 4].copy_from_slice(&spec.pixel(319, 239));
        actual.bgra[3] = 0;
        assert!(world(&actual, spec, 0, 1).is_err());
    }
}
