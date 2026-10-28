//! 有界色值文本转换；HSL/HSV 直接作用于编码 RGB，不进行色彩管理或线性光转换。
use crate::image_tools::{ToolError, ToolResult};

const MAX_TEXT_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ColorFormat {
    Hex,
    Rgb,
    Hsl,
    Hsv,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ColorValue {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub alpha: u8,
}

impl ColorValue {
    pub(super) fn parse(text: &str) -> ToolResult<Self> {
        if text.len() > MAX_TEXT_BYTES {
            return Err(ToolError::Invalid(
                tr!(
                    "色值文本不能超过 256 字节",
                    "Color text cannot exceed 256 bytes"
                )
                .into(),
            ));
        }
        let text = text.trim();
        if let Some(hex) = text.strip_prefix('#') {
            return parse_hex(hex.as_bytes());
        }
        let (name, body) = text.split_once('(').ok_or_else(invalid)?;
        let body = body.strip_suffix(')').ok_or_else(invalid)?;
        let (format, has_alpha) = match name.trim().to_ascii_lowercase().as_str() {
            "rgb" => (ColorFormat::Rgb, false),
            "rgba" => (ColorFormat::Rgb, true),
            "hsl" => (ColorFormat::Hsl, false),
            "hsla" => (ColorFormat::Hsl, true),
            "hsv" => (ColorFormat::Hsv, false),
            "hsva" => (ColorFormat::Hsv, true),
            _ => return Err(invalid()),
        };
        let mut parts = [""; 4];
        let mut count = 0;
        for part in body.split(',') {
            if count == parts.len() || part.trim().is_empty() {
                return Err(invalid());
            }
            parts[count] = part.trim();
            count += 1;
        }
        if count != if has_alpha { 4 } else { 3 } {
            return Err(invalid());
        }
        let alpha = if has_alpha {
            byte(number(parts[3], 1.)?)
        } else {
            255
        };
        if format == ColorFormat::Rgb {
            return Ok(Self {
                r: parts[0].parse().map_err(|_| invalid())?,
                g: parts[1].parse().map_err(|_| invalid())?,
                b: parts[2].parse().map_err(|_| invalid())?,
                alpha,
            });
        }
        let hue = number(parts[0], 360.)?;
        let saturation = percent(parts[1])?;
        let level = percent(parts[2])?;
        let chroma = if format == ColorFormat::Hsl {
            (1. - (2. * level - 1.).abs()) * saturation
        } else {
            level * saturation
        };
        let minimum = if format == ColorFormat::Hsl {
            level - chroma / 2.
        } else {
            level - chroma
        };
        let sector = hue.rem_euclid(360.) / 60.;
        let secondary = chroma * (1. - (sector % 2. - 1.).abs());
        let (r, g, b) = match sector as u8 {
            0 => (chroma, secondary, 0.),
            1 => (secondary, chroma, 0.),
            2 => (0., chroma, secondary),
            3 => (0., secondary, chroma),
            4 => (secondary, 0., chroma),
            _ => (chroma, 0., secondary),
        };
        Ok(Self {
            r: byte(r + minimum),
            g: byte(g + minimum),
            b: byte(b + minimum),
            alpha,
        })
    }

    /// 三位小数使 8 位 RGB/alpha 回读误差小于半个通道级别；输出不会省略非满值 alpha。
    pub(super) fn formatted(self, format: ColorFormat) -> String {
        match format {
            ColorFormat::Hex => {
                let rgb = format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b);
                if self.alpha == 255 {
                    rgb
                } else {
                    format!("{rgb}{:02X}", self.alpha)
                }
            }
            ColorFormat::Rgb => {
                if self.alpha == 255 {
                    format!("rgb({}, {}, {})", self.r, self.g, self.b)
                } else {
                    format!(
                        "rgba({}, {}, {}, {})",
                        self.r,
                        self.g,
                        self.b,
                        decimal(f64::from(self.alpha) / 255.)
                    )
                }
            }
            ColorFormat::Hsl | ColorFormat::Hsv => {
                let r = f64::from(self.r) / 255.;
                let g = f64::from(self.g) / 255.;
                let b = f64::from(self.b) / 255.;
                let maximum = r.max(g).max(b);
                let minimum = r.min(g).min(b);
                let delta = maximum - minimum;
                let hue = if delta == 0. {
                    0.
                } else if maximum == r {
                    60. * ((g - b) / delta).rem_euclid(6.)
                } else if maximum == g {
                    60. * ((b - r) / delta + 2.)
                } else {
                    60. * ((r - g) / delta + 4.)
                };
                let (name, saturation, level) = if format == ColorFormat::Hsl {
                    let lightness = (maximum + minimum) / 2.;
                    let saturation = if delta == 0. {
                        0.
                    } else {
                        delta / (1. - (2. * lightness - 1.).abs())
                    };
                    ("hsl", saturation, lightness)
                } else {
                    let saturation = if maximum == 0. { 0. } else { delta / maximum };
                    ("hsv", saturation, maximum)
                };
                let h = decimal(hue);
                let s = decimal(saturation * 100.);
                let level = decimal(level * 100.);
                if self.alpha == 255 {
                    format!("{name}({h}, {s}%, {level}%)")
                } else {
                    format!(
                        "{name}a({h}, {s}%, {level}%, {})",
                        decimal(f64::from(self.alpha) / 255.)
                    )
                }
            }
        }
    }
}

fn parse_hex(hex: &[u8]) -> ToolResult<ColorValue> {
    let mut rgba = [0, 0, 0, 255];
    match hex.len() {
        3 | 4 => {
            for (channel, digit) in rgba.iter_mut().zip(hex) {
                *channel = hex_digit(*digit).ok_or_else(invalid)? * 17;
            }
        }
        6 | 8 => {
            for (channel, pair) in rgba.iter_mut().zip(hex.chunks_exact(2)) {
                *channel = hex_digit(pair[0]).ok_or_else(invalid)? * 16
                    + hex_digit(pair[1]).ok_or_else(invalid)?;
            }
        }
        _ => return Err(invalid()),
    }
    Ok(ColorValue {
        r: rgba[0],
        g: rgba[1],
        b: rgba[2],
        alpha: rgba[3],
    })
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn number(text: &str, maximum: f64) -> ToolResult<f64> {
    let value = text.trim().parse::<f64>().map_err(|_| invalid())?;
    if !value.is_finite() || !(0. ..=maximum).contains(&value) {
        return Err(invalid());
    }
    Ok(value)
}

fn percent(text: &str) -> ToolResult<f64> {
    Ok(number(text.strip_suffix('%').ok_or_else(invalid)?, 100.)? / 100.)
}

fn byte(value: f64) -> u8 {
    (value.clamp(0., 1.) * 255.).round() as u8
}

fn decimal(value: f64) -> String {
    format!("{value:.3}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn invalid() -> ToolError {
    ToolError::Invalid(
        tr!(
            "色值格式或数值范围无效",
            "Invalid color format or value range"
        )
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(r: u8, g: u8, b: u8, alpha: u8) -> ColorValue {
        ColorValue { r, g, b, alpha }
    }

    #[test]
    fn image_tool_colors_known_primaries_grays_and_alpha_parse_correctly() {
        for (text, expected) in [
            (" #f03 ", rgba(255, 0, 51, 255)),
            ("#0f38", rgba(0, 255, 51, 136)),
            ("#102030", rgba(16, 32, 48, 255)),
            ("#10203040", rgba(16, 32, 48, 64)),
            (" RGB(17, 34, 51) ", rgba(17, 34, 51, 255)),
            ("RGBA(17,34,51,0.5)", rgba(17, 34, 51, 128)),
            ("hsl(0,100%,50%)", rgba(255, 0, 0, 255)),
            ("HSL(360,100%,50%)", rgba(255, 0, 0, 255)),
            ("hsl(120,100%,25%)", rgba(0, 128, 0, 255)),
            ("hsla(240,100%,50%,0.25)", rgba(0, 0, 255, 64)),
            ("hsl(330,0%,50%)", rgba(128, 128, 128, 255)),
            ("hsv(120,100%,100%)", rgba(0, 255, 0, 255)),
            ("hsva(240,100%,50%,0)", rgba(0, 0, 128, 0)),
            ("HSV(300,100%,100%)", rgba(255, 0, 255, 255)),
        ] {
            assert_eq!(ColorValue::parse(text).unwrap(), expected, "{text}");
        }
    }

    #[test]
    fn image_tool_colors_format_preserves_alpha_and_normalizes_gray_hue() {
        let red = rgba(255, 0, 0, 255);
        assert_eq!(red.formatted(ColorFormat::Hex), "#FF0000");
        assert_eq!(red.formatted(ColorFormat::Rgb), "rgb(255, 0, 0)");
        assert_eq!(red.formatted(ColorFormat::Hsl), "hsl(0, 100%, 50%)");
        assert_eq!(red.formatted(ColorFormat::Hsv), "hsv(0, 100%, 100%)");
        let gray = rgba(128, 128, 128, 128);
        assert_eq!(gray.formatted(ColorFormat::Hex), "#80808080");
        assert!(gray.formatted(ColorFormat::Hsl).starts_with("hsla(0, 0%,"));
        assert!(gray.formatted(ColorFormat::Hsv).starts_with("hsva(0, 0%,"));
        assert_eq!(
            gray.formatted(ColorFormat::Rgb),
            "rgba(128, 128, 128, 0.502)"
        );
    }

    #[test]
    fn image_tool_colors_rejects_invalid_syntax_ranges_and_nonfinite_values() {
        for text in [
            "",
            "red",
            "#",
            "#12",
            "#12345",
            "#1234567",
            "#ggg",
            "#１２３",
            "rgb(1,,3)",
            "rgb(1,2,)",
            "rgb(1,2,3,4)",
            "rgba(1,2,3)",
            "rgb(256,0,0)",
            "rgb(-1,0,0)",
            "rgb(1.5,0,0)",
            "rgba(1,2,3,1.001)",
            "rgba(1,2,3,-0.1)",
            "rgba(1,2,3,50%)",
            "rgba(1,2,3,NaN)",
            "rgba(1,2,3,inf)",
            "hsl(361,0%,0%)",
            "hsl(-1,0%,0%)",
            "hsl(NaN,0%,0%)",
            "hsv(inf,0%,0%)",
            "hsl(0,50,50%)",
            "hsv(0,50%,50)",
            "hsl(0,101%,50%)",
            "hsv(0,-1%,50%)",
            "hsl(0,50%,100.01%)",
            "hsv(0,50%,NaN%)",
            "hsva(0,0%,0%,2)",
            "hsl(0,0%,0%,1)",
            "hsl(0:0%:0%)",
            "rgb(0,0,0))",
            "rgb((0,0,0)",
        ] {
            assert!(ColorValue::parse(text).is_err(), "{text}");
        }
        assert!(ColorValue::parse(&" ".repeat(MAX_TEXT_BYTES + 1)).is_err());
    }

    #[test]
    fn image_tool_colors_all_formats_roundtrip_representative_eight_bit_values() {
        for [r, g, b] in [
            [0, 0, 0],
            [255, 255, 255],
            [1, 2, 3],
            [17, 127, 254],
            [128, 127, 129],
            [254, 1, 2],
            [30, 180, 90],
            [250, 251, 252],
            [0, 255, 255],
            [255, 0, 255],
            [255, 255, 0],
            [66, 0, 253],
        ] {
            for alpha in [0, 1, 127, 128, 254, 255] {
                let color = rgba(r, g, b, alpha);
                for format in [
                    ColorFormat::Hex,
                    ColorFormat::Rgb,
                    ColorFormat::Hsl,
                    ColorFormat::Hsv,
                ] {
                    let text = color.formatted(format);
                    assert_eq!(ColorValue::parse(&text).unwrap(), color, "{text}");
                }
            }
        }
    }
}
