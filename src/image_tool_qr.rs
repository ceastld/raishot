//! 二维码像素生成；编码与掩模选择由固定依赖负责，四周保留完整静区。
use crate::{
    capture::Image,
    image_tools::{
        allocate_pixels, check_cancel, validate_output, ToolError, ToolLimits, ToolResult,
    },
};
use std::sync::atomic::AtomicBool;

pub(crate) const MAX_TEXT_BYTES: usize = 4096;
const QUIET_MODULES: u32 = 4;

/// UTF-8 原文编码；返回值不持有文本。成品固定为不透明深色码白底，不跟随界面主题。
pub(crate) fn generate(
    text: &str,
    module_pixels: u32,
    limits: ToolLimits,
    cancel: &AtomicBool,
) -> ToolResult<Image> {
    check_cancel(cancel)?;
    if text.is_empty() || text.len() > MAX_TEXT_BYTES {
        return Err(ToolError::Invalid(trf!(
            "二维码内容不能为空，且 UTF-8 编码不能超过 {MAX_TEXT_BYTES} 字节",
            "QR content must be non-empty and at most {MAX_TEXT_BYTES} UTF-8 bytes"
        )));
    }
    if ![2, 4, 8, 16].contains(&module_pixels) {
        return Err(ToolError::Invalid(
            tr!(
                "二维码模块倍数须为 2、4、8 或 16",
                "QR module size must be 2, 4, 8 or 16"
            )
            .into(),
        ));
    }
    let code = encode_text(text, cancel)?;
    check_cancel(cancel)?;
    let modules = u32::try_from(code.width()).map_err(|_| size_error())?;
    let side = modules
        .checked_add(QUIET_MODULES * 2)
        .and_then(|modules| modules.checked_mul(module_pixels))
        .ok_or_else(size_error)?;
    let byte_len = validate_output(side, side, limits)?;
    check_cancel(cancel)?;
    let mut bgra = allocate_pixels(byte_len)?;
    let stride = side as usize * 4;
    for (y, row) in bgra.chunks_exact_mut(stride).enumerate() {
        check_cancel(cancel)?;
        row.fill(255);
        let Some(module_y) = (y as u32 / module_pixels).checked_sub(QUIET_MODULES) else {
            continue;
        };
        if module_y >= modules {
            continue;
        }
        for module_x in 0..modules {
            if code[(module_x as usize, module_y as usize)] != qrcode::Color::Dark {
                continue;
            }
            let start = ((module_x + QUIET_MODULES) * module_pixels) as usize * 4;
            let length = module_pixels as usize * 4;
            for pixel in row[start..start + length].chunks_exact_mut(4) {
                pixel[..3].fill(0);
            }
        }
    }
    check_cancel(cancel)?;
    Ok(Image {
        width: side,
        height: side,
        bgra,
        readback_ms: 0.,
    })
}

fn encode_text(text: &str, cancel: &AtomicBool) -> ToolResult<qrcode::QrCode> {
    if text.is_ascii() {
        return qrcode::QrCode::new(text.as_bytes()).map_err(encoding_error);
    }

    // 非 ASCII 明确声明 UTF-8，并只使用 Byte 模式，避免将 UTF-8 字节对误作 Kanji。
    // ECI 和 Byte 段头也占容量，由编码器逐个普通版本检查，不能按文本字节数猜版本。
    for version in 1..=40 {
        check_cancel(cancel)?;
        let mut bits = qrcode::bits::Bits::new(qrcode::Version::Normal(version));
        let prepared = bits
            .push_eci_designator(26)
            .and_then(|()| bits.push_byte_data(text.as_bytes()))
            .and_then(|()| bits.push_terminator(qrcode::EcLevel::M));
        match prepared {
            Ok(()) => {
                check_cancel(cancel)?;
                return qrcode::QrCode::with_bits(bits, qrcode::EcLevel::M).map_err(encoding_error);
            }
            Err(qrcode::types::QrError::DataTooLong) => continue,
            Err(error) => return Err(encoding_error(error)),
        }
    }
    Err(encoding_error(qrcode::types::QrError::DataTooLong))
}

fn encoding_error(error: qrcode::types::QrError) -> ToolError {
    ToolError::Invalid(trf!(
        "二维码内容超出编码容量或无法编码：{error}",
        "QR content exceeds encoder capacity or cannot be encoded: {error}"
    ))
}

fn size_error() -> ToolError {
    ToolError::Invalid(tr!("二维码尺寸溢出", "QR image dimensions overflow").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_tool_qr_generated_utf8_is_readable_by_existing_recognition() {
        for text in [
            "あい",
            "こんにちは",
            "😀🧪🚀",
            "二维码 UTF-8 https://example.com/?q=你好",
            "Mixed text 中英混合 123 日本語",
        ] {
            let image = generate(text, 8, ToolLimits::default(), &AtomicBool::new(false)).unwrap();
            let decoded = decode_pixels(&image);
            assert_utf8_payload(&decoded, text);
            let hits = crate::recognition::qr_codes(image);
            assert!(hits.iter().any(|hit| hit.text == text), "{text:?}");
        }
    }

    fn decode_pixels(image: &Image) -> quircs::Data {
        let gray: Vec<_> = image.bgra.chunks_exact(4).map(|pixel| pixel[0]).collect();
        let mut decoder = quircs::Quirc::default();
        let mut codes = decoder.identify(image.width as usize, image.height as usize, &gray);
        let decoded = codes
            .next()
            .expect("one QR code must be located")
            .expect("QR extraction must succeed")
            .decode()
            .expect("QR payload must decode");
        assert!(codes.next().is_none(), "one QR code must be generated");
        decoded
    }

    fn assert_utf8_payload(decoded: &quircs::Data, text: &str) {
        assert_eq!(decoded.eci, Some(quircs::Eci::Utf8), "{text:?}");
        assert_eq!(decoded.data_type, Some(quircs::DataType::Byte), "{text:?}");
        assert_eq!(decoded.payload, text.as_bytes(), "{text:?}");
        assert_eq!(std::str::from_utf8(&decoded.payload).unwrap(), text);
    }

    #[test]
    fn image_tool_qr_utf8_eci_byte_capacity_includes_segment_headers() {
        // Version 40/M 有 18672 数据位，ECI 与 Byte 段头共 32 位，最多容纳 2330 字节。
        let text = format!("あ{}", "a".repeat(2327));
        assert_eq!(text.len(), 2330);
        assert!(text.len() < MAX_TEXT_BYTES);
        let image = generate(&text, 4, ToolLimits::default(), &AtomicBool::new(false)).unwrap();
        assert_eq!((image.width, image.height), ((177 + 8) * 4, (177 + 8) * 4));
        let decoded = decode_pixels(&image);
        assert_eq!(decoded.version, 40);
        assert_eq!(decoded.ecc_level, quircs::EccLevel::M);
        assert_utf8_payload(&decoded, &text);

        let too_long = format!("{text}a");
        assert_eq!(too_long.len(), 2331);
        assert!(too_long.len() < MAX_TEXT_BYTES);
        assert!(matches!(
            generate(&too_long, 4, ToolLimits::default(), &AtomicBool::new(false)),
            Err(ToolError::Invalid(_))
        ));
    }

    #[test]
    fn image_tool_qr_quiet_zone_and_module_pixels_are_exact() {
        let text = "quiet zone";
        let modules = qrcode::QrCode::new(text.as_bytes()).unwrap().width() as u32;
        for scale in [2, 4, 8, 16] {
            let image =
                generate(text, scale, ToolLimits::default(), &AtomicBool::new(false)).unwrap();
            let quiet = QUIET_MODULES * scale;
            assert_eq!(
                (image.width, image.height),
                ((modules + 8) * scale, (modules + 8) * scale)
            );
            for y in 0..image.height {
                for x in 0..image.width {
                    let offset = (y as usize * image.width as usize + x as usize) * 4;
                    let pixel = &image.bgra[offset..offset + 4];
                    assert_eq!(pixel[3], 255);
                    if x < quiet
                        || y < quiet
                        || x >= image.width - quiet
                        || y >= image.height - quiet
                    {
                        assert_eq!(pixel, [255; 4]);
                    }
                }
            }
            let first_dark = (quiet as usize * image.width as usize + quiet as usize) * 4;
            assert_eq!(&image.bgra[first_dark..first_dark + 4], [0, 0, 0, 255]);
        }
    }

    #[test]
    fn image_tool_qr_rejects_invalid_text_capacity_scale_budget_and_cancel() {
        let cancel = AtomicBool::new(false);
        for text in [String::new(), "你".repeat(1366), "a".repeat(MAX_TEXT_BYTES)] {
            assert!(generate(&text, 4, ToolLimits::default(), &cancel).is_err());
        }
        for scale in [0, 1, 3, u32::MAX] {
            assert!(generate("test", scale, ToolLimits::default(), &cancel).is_err());
        }
        assert!(generate(
            "test",
            4,
            ToolLimits {
                max_output_pixels: 1,
                ..ToolLimits::default()
            },
            &cancel
        )
        .is_err());
        assert!(generate(
            "test",
            4,
            ToolLimits {
                max_side: 1,
                ..ToolLimits::default()
            },
            &cancel
        )
        .is_err());
        assert!(matches!(
            generate("test", 4, ToolLimits::default(), &AtomicBool::new(true)),
            Err(ToolError::Cancelled)
        ));
    }
}
