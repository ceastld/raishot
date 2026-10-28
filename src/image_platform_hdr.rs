//! 已证明为全范围、直alpha编码RGB的高精度像素，按固定显示策略生成SDR副本。
//! 本模块不证明系统解码器的像素域；调用方须先确认其输入合同。
//! PQ/HLG解传递与线性原色转换遵循各自定义，亮度肩部和色域裁切是RaiShot策略，
//! 不代表标准HDR映射、显示器校准或原生解码精度已经验收。
use super::{
    coded::{budget, check_cancel, checked_pixels, invalid, unsupported},
    Cicp, PlatformError, MAX_BYTES,
};
use crate::capture::Image;
use image::metadata::Orientation;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Policy {
    /// SDR参考峰值100nit；亮度不高于75nit时保留，之上采用平滑有理肩部。
    Sdr100Knee75,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Primaries {
    Bt709,
    Bt2020,
}

impl Primaries {
    fn luminance(self, rgb: [f64; 3]) -> f64 {
        let weights = match self {
            Self::Bt709 => [0.2126, 0.7152, 0.0722],
            Self::Bt2020 => [0.2627, 0.6780, 0.0593],
        };
        rgb[0] * weights[0] + rgb[1] * weights[1] + rgb[2] * weights[2]
    }

    fn to_bt709(self, rgb: [f64; 3]) -> [f64; 3] {
        match self {
            Self::Bt709 => rgb,
            // 两组原色均为D65；先转线性原色，不另施加白点适应。
            Self::Bt2020 => [
                1.660_491_002_108_434_3 * rgb[0]
                    - 0.587_641_138_788_549_6 * rgb[1]
                    - 0.072_849_863_319_884_8 * rgb[2],
                -0.124_550_474_521_590_85 * rgb[0] + 1.132_899_897_125_960_3 * rgb[1]
                    - 0.008_349_422_604_369_473 * rgb[2],
                -0.018_150_763_354_905_307 * rgb[0] - 0.100_578_898_008_007_37 * rgb[1]
                    + 1.118_729_661_362_912_7 * rgb[2],
            ],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transfer {
    Pq,
    Hlg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Plan {
    primaries: Primaries,
    transfer: Transfer,
    policy: Policy,
}

fn bad_pixel() -> PlatformError {
    invalid(
        "HDR像素或数值计算无效",
        "Invalid HDR pixel or numerical result",
    )
}

fn unit_signal(value: f64) -> Result<f64, PlatformError> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(bad_pixel());
    }
    Ok(value)
}

// BT.2100的PQ EOTF；输出为绝对亮度nit，不对每通道单独做tone mapping。
fn pq_nits(value: f64) -> Result<f64, PlatformError> {
    let value = unit_signal(value)?;
    let power = value.powf(32.0 / 2523.0);
    let denominator = 2413.0 / 128.0 - 2392.0 / 128.0 * power;
    if denominator <= 0.0 {
        return Err(bad_pixel());
    }
    let linear =
        ((power - 3424.0 / 4096.0).max(0.0) / denominator).powf(16384.0 / 2610.0) * 10000.0;
    if !linear.is_finite() || linear < 0.0 {
        return Err(bad_pixel());
    }
    Ok(linear.min(10000.0))
}

fn hlg_scene(value: f64) -> Result<f64, PlatformError> {
    let value = unit_signal(value)?;
    let linear = if value <= 0.5 {
        value * value / 3.0
    } else {
        let a: f64 = 0.178_832_77;
        let b = 1.0 - 4.0 * a;
        let c = 0.5 - a * (4.0 * a).ln();
        (((value - c) / a).exp() + b) / 12.0
    };
    if !linear.is_finite() || linear < 0.0 {
        return Err(bad_pixel());
    }
    // 有限精度运算可让端点1略微超出1；输入合同的场景范围仍是[0,1]。
    Ok(linear.min(1.0))
}

fn hlg_display(scene: [f64; 3], primaries: Primaries) -> Result<[f64; 3], PlatformError> {
    for value in scene {
        unit_signal(value)?;
    }
    let luminance = primaries.luminance(scene);
    // BT.2100的HLG参考原色为BT.2020；CP1使用BT.709亮度权重，是本显示策略的扩展。
    // 固定1000nit参考峰值、系统gamma1.2、黑电平0；不读取系统显示器或环境。
    // 三通道共用场景亮度增益，不能用逐通道pow(1.2)替代OOTF。
    let gain = if luminance == 0.0 {
        0.0
    } else {
        1000.0 * luminance.powf(0.2)
    };
    Ok([scene[0] * gain, scene[1] * gain, scene[2] * gain])
}

fn knee_nits(value: f64, policy: Policy) -> Result<f64, PlatformError> {
    if !value.is_finite() || value < 0.0 {
        return Err(bad_pixel());
    }
    Ok(match policy {
        Policy::Sdr100Knee75 if value <= 75.0 => value,
        Policy::Sdr100Knee75 => 75.0 + 25.0 * ((value - 75.0) / (value - 50.0)),
    })
}

fn srgb_byte(linear: f64) -> Result<u8, PlatformError> {
    if !linear.is_finite() {
        return Err(bad_pixel());
    }
    // 肩部共同缩放后才转原色、裁切越界色域；裁切可能改变饱和色的色相。
    let linear = linear.clamp(0.0, 1.0);
    let encoded = if linear <= 0.003_130_8 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    Ok((encoded * 255.0).round().clamp(0.0, 255.0) as u8)
}

fn alpha_byte(alpha: u16) -> u8 {
    ((u32::from(alpha) * 255 + 32767) / 65535) as u8
}

fn visible_budget(resident_bytes: u64, output_capacity: u64) -> Result<(), PlatformError> {
    resident_bytes
        .checked_add(output_capacity)
        .filter(|bytes| *bytes <= MAX_BYTES)
        .ok_or_else(budget)?;
    Ok(())
}

struct Layout {
    output_bytes: usize,
    display_dimensions: (u32, u32),
}

fn layout(
    dimensions: (u32, u32),
    orientation: Orientation,
    resident_bytes: u64,
    max_output_bytes: u64,
) -> Result<Layout, PlatformError> {
    let pixels = checked_pixels(dimensions, max_output_bytes)?;
    let output_bytes = pixels.checked_mul(4).ok_or_else(budget)?;
    visible_budget(resident_bytes, output_bytes)?;
    let swapped = matches!(
        orientation,
        Orientation::Rotate90FlipH
            | Orientation::Rotate90
            | Orientation::Rotate270FlipH
            | Orientation::Rotate270
    );
    Ok(Layout {
        output_bytes: usize::try_from(output_bytes).map_err(|_| budget())?,
        display_dimensions: if swapped {
            (dimensions.1, dimensions.0)
        } else {
            dimensions
        },
    })
}

impl Plan {
    pub(super) fn new(
        cicp: Cicp,
        policy: Policy,
        icc_present: bool,
    ) -> Result<Self, PlatformError> {
        if icc_present {
            return Err(unsupported(
                "存在ICC配置时不能用CICP替代HDR颜色映射",
                "CICP HDR mapping cannot replace an embedded ICC profile",
            ));
        }
        let primaries = match cicp.primaries {
            1 => Primaries::Bt709,
            9 => Primaries::Bt2020,
            _ => return Err(unsupported("HDR原色暂不支持", "Unsupported HDR primaries")),
        };
        let transfer = match cicp.transfer {
            16 => Transfer::Pq,
            18 => Transfer::Hlg,
            _ => {
                return Err(unsupported(
                    "HDR传递函数暂不支持",
                    "Unsupported HDR transfer function",
                ))
            }
        };
        // matrix及full_range描述原编码平面。此处输入已是全范围编码RGB，不能再套一次。
        Ok(Self {
            primaries,
            transfer,
            policy,
        })
    }

    fn rgb_nits(self, encoded: [f64; 3]) -> Result<[f64; 3], PlatformError> {
        match self.transfer {
            Transfer::Pq => Ok([
                pq_nits(encoded[0])?,
                pq_nits(encoded[1])?,
                pq_nits(encoded[2])?,
            ]),
            Transfer::Hlg => hlg_display(
                [
                    hlg_scene(encoded[0])?,
                    hlg_scene(encoded[1])?,
                    hlg_scene(encoded[2])?,
                ],
                self.primaries,
            ),
        }
    }

    fn map_rgb(self, encoded: [f64; 3]) -> Result<[u8; 3], PlatformError> {
        let rgb = self.rgb_nits(encoded)?;
        let luminance = self.primaries.luminance(rgb);
        if luminance == 0.0 {
            return Ok([0; 3]);
        }
        let scale = knee_nits(luminance, self.policy)? / luminance / 100.0;
        let rgb = self.primaries.to_bt709(rgb);
        Ok([
            srgb_byte(rgb[0] * scale)?,
            srgb_byte(rgb[1] * scale)?,
            srgb_byte(rgb[2] * scale)?,
        ])
    }

    /// 输入须是全范围、little-endian unsigned16、R'G'B'A、直alpha；不接受行padding。
    /// resident_bytes须包含该输入的完整实际capacity以及仍驻留的来源、目录等可见缓冲。
    pub(super) fn render_rgba16(
        self,
        encoded: &[u8],
        dimensions: (u32, u32),
        orientation: Orientation,
        resident_bytes: u64,
        max_output_bytes: u64,
        cancel: &AtomicBool,
    ) -> Result<Image, PlatformError> {
        check_cancel(cancel)?;
        let pixels = checked_pixels(dimensions, max_output_bytes)?;
        let input_bytes = pixels.checked_mul(8).ok_or_else(budget)?;
        if encoded.len() as u64 != input_bytes {
            return Err(invalid(
                "HDR像素长度不一致",
                "Inconsistent HDR pixel length",
            ));
        }
        if resident_bytes < input_bytes {
            return Err(budget());
        }
        self.render_encoded_rgb16(
            dimensions,
            orientation,
            resident_bytes,
            max_output_bytes,
            cancel,
            |x, y| {
                let offset = ((u64::from(y) * u64::from(dimensions.0) + u64::from(x)) * 8) as usize;
                let pixel = &encoded[offset..offset + 8];
                Ok([
                    u16::from_le_bytes([pixel[0], pixel[1]]),
                    u16::from_le_bytes([pixel[2], pixel[3]]),
                    u16::from_le_bytes([pixel[4], pixel[5]]),
                    u16::from_le_bytes([pixel[6], pixel[7]]),
                ])
            },
        )
    }

    /// sample以未定向显示窗口坐标返回全范围编码RGB16及直alpha，不再做YCC范围或矩阵换算。
    /// resident_bytes包含sample借用的实际底层缓冲；只分配一份最终BGRA8，不缓存整图float。
    pub(super) fn render_encoded_rgb16(
        self,
        dimensions: (u32, u32),
        orientation: Orientation,
        resident_bytes: u64,
        max_output_bytes: u64,
        cancel: &AtomicBool,
        mut sample: impl FnMut(u32, u32) -> Result<[u16; 4], PlatformError>,
    ) -> Result<Image, PlatformError> {
        check_cancel(cancel)?;
        let layout = layout(dimensions, orientation, resident_bytes, max_output_bytes)?;
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(layout.output_bytes)
            .map_err(|_| budget())?;
        visible_budget(resident_bytes, bgra.capacity() as u64)?;
        bgra.resize(layout.output_bytes, 0);
        check_cancel(cancel)?;
        let (width, height) = dimensions;
        let (display_width, display_height) = layout.display_dimensions;
        for y in 0..height {
            check_cancel(cancel)?;
            for x in 0..width {
                let pixel = sample(x, y);
                // sample可同时置取消并返回其它错误；不得让其错误覆盖取消。
                check_cancel(cancel)?;
                let [r, g, b, a] = pixel?;
                let rgb = self.map_rgb([
                    f64::from(r) / 65535.0,
                    f64::from(g) / 65535.0,
                    f64::from(b) / 65535.0,
                ])?;
                let (target_x, target_y) = match orientation {
                    Orientation::NoTransforms => (x, y),
                    Orientation::FlipHorizontal => (width - 1 - x, y),
                    Orientation::Rotate180 => (width - 1 - x, height - 1 - y),
                    Orientation::FlipVertical => (x, height - 1 - y),
                    Orientation::Rotate90FlipH => (y, x),
                    Orientation::Rotate90 => (height - 1 - y, x),
                    Orientation::Rotate270FlipH => (height - 1 - y, width - 1 - x),
                    Orientation::Rotate270 => (y, width - 1 - x),
                };
                let offset = ((u64::from(target_y) * u64::from(display_width)
                    + u64::from(target_x))
                    * 4) as usize;
                bgra[offset..offset + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], alpha_byte(a)]);
            }
        }
        check_cancel(cancel)?;
        Ok(Image {
            width: display_width,
            height: display_height,
            bgra,
            readback_ms: 0.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn signal(primaries: u16, transfer: u16) -> Cicp {
        Cicp {
            primaries,
            transfer,
            matrix: 9,
            full_range: true,
        }
    }

    fn plan(primaries: u16, transfer: u16) -> Plan {
        Plan::new(signal(primaries, transfer), Policy::Sdr100Knee75, false).unwrap()
    }

    fn close(actual: f64, expected: f64, tolerance: f64) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "actual={actual:.15}, expected={expected:.15}"
        );
    }

    #[test]
    fn image_platform_hdr_pq_absolute_luminance_matches_independent_reference_points() {
        for (encoded, nits, byte) in [
            (0.0, 0.0, 0),
            (0.508_078_421_517_399, 100.0, 240),
            (0.751_827_096_247_041, 1000.0, 254),
            (1.0, 10000.0, 255),
        ] {
            close(pq_nits(encoded).unwrap(), nits, 0.000_001);
            assert_eq!(plan(9, 16).map_rgb([encoded; 3]).unwrap(), [byte; 3]);
        }
        close(knee_nits(100.0, Policy::Sdr100Knee75).unwrap(), 87.5, 1e-12);
        close(
            knee_nits(1000.0, Policy::Sdr100Knee75).unwrap(),
            99.342_105_263_157_89,
            1e-12,
        );
        close(
            knee_nits(10000.0, Policy::Sdr100Knee75).unwrap(),
            99.937_185_929_648_24,
            1e-12,
        );
        close(knee_nits(75.0, Policy::Sdr100Knee75).unwrap(), 75.0, 0.0);
    }

    #[test]
    fn image_platform_hdr_hlg_breakpoint_and_shared_luminance_ootf_match_reference() {
        close(hlg_scene(0.0).unwrap(), 0.0, 0.0);
        close(hlg_scene(0.5).unwrap(), 0.083_333_333_333_333_33, 1e-15);
        close(hlg_scene(0.75).unwrap(), 0.264_962_560_421_007_24, 1e-14);
        for (encoded, nits) in [
            (0.5, 50.697_028_491_100_475),
            (0.75, 203.152_145_937_545_38),
            (1.0, 1000.0),
        ] {
            for actual in plan(9, 18).rgb_nits([encoded; 3]).unwrap() {
                close(actual, nits, 1e-9);
            }
        }
        let rgb = hlg_display([1.0, 0.25, 0.1], Primaries::Bt2020).unwrap();
        for (actual, expected) in rgb.into_iter().zip([
            847.853_072_470_644_6,
            211.963_268_117_661_16,
            84.785_307_247_064_47,
        ]) {
            close(actual, expected, 1e-9);
        }
    }

    #[test]
    fn image_platform_hdr_linear_primaries_keep_d65_and_clip_only_after_conversion() {
        let rgb = Primaries::Bt2020.to_bt709([100.0, 50.0, 25.0]);
        for (actual, expected) in rgb.into_iter().zip([
            134.845_796_688_418_8,
            43.981_211_839_029_69,
            21.124_220_298_181_918,
        ]) {
            close(actual, expected, 1e-11);
        }
        for actual in Primaries::Bt2020.to_bt709([1.0; 3]) {
            close(actual, 1.0, 1e-14);
        }
        let red = Primaries::Bt2020.to_bt709([1.0, 0.0, 0.0]);
        assert!(red[0] > 1.0 && red[1] < 0.0 && red[2] < 0.0);
        assert_eq!(red.map(|value| srgb_byte(value).unwrap()), [255, 0, 0]);
        assert_eq!(srgb_byte(0.0).unwrap(), 0);
        assert_eq!(srgb_byte(0.003_130_8).unwrap(), 10);
        assert_eq!(srgb_byte(1.0).unwrap(), 255);
    }

    #[test]
    fn image_platform_hdr_rgba16_keeps_color_and_straight_alpha_through_all_orientations() {
        let pixels = [
            [65535, 0, 0, 0],
            [0, 65535, 0, 32768],
            [0, 0, 65535, 65535],
            [65535, 65535, 0, 13107],
            [0, 65535, 65535, 26214],
            [65535, 65535, 65535, 52428],
        ];
        let encoded: Vec<u8> = pixels
            .iter()
            .flatten()
            .flat_map(|value: &u16| value.to_le_bytes())
            .collect();
        let colors = [
            [0, 0, 255, 0],
            [0, 255, 0, 128],
            [255, 0, 0, 255],
            [0, 255, 255, 51],
            [255, 255, 0, 102],
            [255, 255, 255, 204],
        ];
        for (orientation, dimensions, order) in [
            (1, (3, 2), [0, 1, 2, 3, 4, 5]),
            (2, (3, 2), [2, 1, 0, 5, 4, 3]),
            (3, (3, 2), [5, 4, 3, 2, 1, 0]),
            (4, (3, 2), [3, 4, 5, 0, 1, 2]),
            (5, (2, 3), [0, 3, 1, 4, 2, 5]),
            (6, (2, 3), [3, 0, 4, 1, 5, 2]),
            (7, (2, 3), [5, 2, 4, 1, 3, 0]),
            (8, (2, 3), [2, 5, 1, 4, 0, 3]),
        ] {
            let image = plan(1, 16)
                .render_rgba16(
                    &encoded,
                    (3, 2),
                    Orientation::from_exif(orientation).unwrap(),
                    encoded.capacity() as u64,
                    24,
                    &AtomicBool::new(false),
                )
                .unwrap();
            assert_eq!((image.width, image.height), dimensions);
            let expected: Vec<u8> = order.into_iter().flat_map(|index| colors[index]).collect();
            assert_eq!(image.bgra, expected, "orientation={orientation}");
        }
        assert_eq!(alpha_byte(0), 0);
        assert_eq!(alpha_byte(32768), 128);
        assert_eq!(alpha_byte(65535), 255);
    }

    #[test]
    fn image_platform_hdr_encoded_rgb_contract_never_repeats_source_ycc_range_or_matrix() {
        let first = plan(9, 16);
        let second = Plan::new(
            Cicp {
                full_range: false,
                matrix: 6,
                ..signal(9, 16)
            },
            Policy::Sdr100Knee75,
            false,
        )
        .unwrap();
        let input = [42000, 36000, 32000, 32768];
        let render = |plan: Plan| {
            plan.render_encoded_rgb16(
                (1, 1),
                Orientation::NoTransforms,
                0,
                4,
                &AtomicBool::new(false),
                |_, _| Ok(input),
            )
            .unwrap()
        };
        assert_eq!(render(first).bgra, render(second).bgra);
        assert_eq!(render(first).bgra[3], 128);
    }

    #[test]
    fn image_platform_hdr_rejects_unproven_profiles_unknown_signals_and_non_finite_math() {
        for cicp in [signal(2, 16), signal(12, 18), signal(9, 13), signal(9, 2)] {
            assert!(matches!(
                Plan::new(cicp, Policy::Sdr100Knee75, false),
                Err(PlatformError::Unsupported { .. })
            ));
        }
        assert!(matches!(
            Plan::new(signal(9, 16), Policy::Sdr100Knee75, true),
            Err(PlatformError::Unsupported { .. })
        ));
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.01, 1.01] {
            assert!(pq_nits(bad).is_err());
            assert!(hlg_scene(bad).is_err());
        }
        assert!(knee_nits(f64::NAN, Policy::Sdr100Knee75).is_err());
        assert!(srgb_byte(f64::NAN).is_err());
        assert!(hlg_display([1.0, f64::NAN, 0.0], Primaries::Bt2020).is_err());
    }

    #[test]
    fn image_platform_hdr_budgets_use_resident_capacity_and_exact_pixel_layout_before_sampling() {
        let token = AtomicBool::new(false);
        assert!(layout((2, 2), Orientation::NoTransforms, MAX_BYTES - 16, 16).is_ok());
        assert!(matches!(
            layout((2, 2), Orientation::NoTransforms, MAX_BYTES - 15, 16),
            Err(PlatformError::Budget(_))
        ));
        assert!(visible_budget(MAX_BYTES - 16, 17).is_err());
        for (dimensions, resident, output) in [
            ((0, 1), 0, 4),
            ((65536, 1), 0, MAX_BYTES),
            ((8001, 4000), 0, MAX_BYTES),
            ((2, 2), u64::MAX, 16),
            ((2, 2), 0, 15),
        ] {
            assert!(plan(9, 16)
                .render_encoded_rgb16(
                    dimensions,
                    Orientation::NoTransforms,
                    resident,
                    output,
                    &token,
                    |_, _| panic!("invalid budget must not sample")
                )
                .is_err());
        }
        for (length, resident) in [(31, 32), (33, 33), (32, 31)] {
            assert!(plan(9, 16)
                .render_rgba16(
                    &vec![0; length],
                    (2, 2),
                    Orientation::NoTransforms,
                    resident,
                    16,
                    &token
                )
                .is_err());
        }
    }

    #[test]
    fn image_platform_hdr_cancel_drops_partial_output_and_wins_over_sampler_error() {
        let cancel = AtomicBool::new(true);
        assert!(matches!(
            plan(9, 16).render_encoded_rgb16(
                (0, 0),
                Orientation::NoTransforms,
                u64::MAX,
                0,
                &cancel,
                |_, _| panic!("canceled before sampling")
            ),
            Err(PlatformError::Cancelled)
        ));
        for fail in [false, true] {
            let cancel = AtomicBool::new(false);
            let mut calls = 0;
            let result = plan(9, 16).render_encoded_rgb16(
                (513, 1),
                Orientation::NoTransforms,
                0,
                513 * 4,
                &cancel,
                |_, _| {
                    calls += 1;
                    if calls == 257 {
                        cancel.store(true, Ordering::Relaxed);
                        if fail {
                            return Err(bad_pixel());
                        }
                    }
                    Ok([65535; 4])
                },
            );
            assert!(matches!(result, Err(PlatformError::Cancelled)));
            assert_eq!(calls, 257);
        }
    }
}
