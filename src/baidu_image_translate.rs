//! 百度翻译开放平台「图片翻译 V2.0」：上传选区图片，取回服务端擦除重绘后的贴合图。
//! 只在用户选择百度引擎并点击图片翻译时发送当前选区图片；凭证只存本机配置。
//! 接口：`POST /ait/api/picture/translate`，`Authorization: Bearer <API Key>`，JSON 体带 `appid`。
use crate::capture::Image;
use std::time::Duration;

pub(crate) const ENDPOINT: &str = "https://fanyi-api.baidu.com/ait/api/picture/translate";
/// 原图上限 5MB；留出余量后按长边缩放重编码。
const MAX_BYTES: usize = 3 * 1024 * 1024;
const MAX_LONG_EDGE: u32 = 4096;
const MIN_SHORT_EDGE: u32 = 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub app_id: String,
    pub api_key: String,
}
impl Credentials {
    pub(crate) fn from_preferences(preferences: &crate::preferences::Preferences) -> Option<Self> {
        let (app_id, api_key) = (
            preferences.baidu_app_id.trim(),
            preferences.baidu_api_key.trim(),
        );
        (!app_id.is_empty() && !api_key.is_empty()).then(|| Self {
            app_id: app_id.to_owned(),
            api_key: api_key.to_owned(),
        })
    }
}

/// 本次图片翻译使用的引擎；只有显式选择图片引擎才上传整张选区。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Engine {
    Baidu(Credentials),
    Local,
}
pub(crate) fn engine(preferences: &crate::preferences::Preferences) -> Engine {
    let credentials = Credentials::from_preferences(preferences);
    match (preferences.image_translate_engine.trim(), credentials) {
        ("baidu", Some(credentials)) => Engine::Baidu(credentials),
        _ => Engine::Local,
    }
}

/// 设置里的目标语言（auto / zh-CN / en / ja / 语言名…）映射为百度 V2 语种码。
pub(crate) fn target_code(target: &str) -> &'static str {
    let lower = target.trim().to_ascii_lowercase();
    match lower.as_str() {
        "" | "auto" => {
            if crate::i18n::is_english() {
                "en"
            } else {
                "zh"
            }
        }
        "en" | "english" | "英语" | "英文" => "en",
        "ja" | "jp" | "japanese" | "日语" | "日文" => "jp",
        "ko" | "kor" | "korean" | "韩语" => "kor",
        "fr" | "fra" | "french" | "法语" => "fra",
        "es" | "spa" | "spanish" | "西班牙语" => "spa",
        "de" | "german" | "德语" => "de",
        "ru" | "russian" | "俄语" => "ru",
        "it" | "italian" | "意大利语" => "it",
        "pt" | "portuguese" | "葡萄牙语" => "pt",
        "ar" | "ara" | "arabic" | "阿拉伯语" => "ara",
        "vi" | "vie" | "vietnamese" | "越南语" => "vie",
        "th" | "thai" | "泰语" => "th",
        "id" | "indonesian" | "印尼语" => "id",
        "ms" | "may" | "malay" | "马来语" => "may",
        "nl" | "dutch" | "荷兰语" => "nl",
        "pl" | "polish" | "波兰语" => "pl",
        "tr" | "turkish" | "土耳其语" => "tr",
        // V2 附录只列 zh；繁体也按 zh 请求。
        _ => "zh",
    }
}

/// 上传选区图片并返回与原图同尺寸的贴合图。阻塞调用，需在后台线程执行。
pub(crate) fn translate(
    credentials: &Credentials,
    image: &Image,
    target: &str,
) -> Result<Image, String> {
    let jpeg = encode_upload(image)?;
    let body = request_body(credentials, &base64_encode(&jpeg), target_code(target));
    let agent = crate::ppocr::assets::download_agent().map_err(|error| error.to_string())?;
    let response = agent
        .post(ENDPOINT)
        .timeout(Duration::from_secs(60))
        .set("User-Agent", crate::brand::USER_AGENT)
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {}", credentials.api_key))
        .send_string(&body.to_string());
    let text = match response {
        Ok(response) => response.into_string().map_err(|_| {
            tr!("读取百度翻译结果失败", "Failed to read the Baidu result").to_owned()
        })?,
        Err(ureq::Error::Status(status, response)) => {
            let text = response.into_string().unwrap_or_default();
            if let Err(error) = parse_response(&text) {
                return Err(error);
            }
            return Err(trf!(
                "百度图片翻译返回 HTTP {}",
                "Baidu image translation returned HTTP {}",
                status
            ));
        }
        Err(ureq::Error::Transport(_)) => {
            return Err(tr!(
                "无法连接百度翻译服务；请检查网络或代理",
                "Cannot reach Baidu Translate; check network or proxy"
            )
            .into())
        }
    };
    let pasted = parse_response(&text)?;
    decode_pasted(&pasted, image.width, image.height)
}

fn request_body(credentials: &Credentials, content: &str, to: &str) -> serde_json::Value {
    serde_json::json!({
        "from": "auto",
        "to": to,
        "appid": credentials.app_id,
        "content": content,
        // 1 = 返回整图贴合图；view_type 1 = 高精擦除。
        "paste": 1,
        "need_intervene": 0,
        "view_type": 1,
        "model_type": "nmt",
    })
}

/// 成功返回 `paste_img` 的 base64；失败映射为可读错误。
fn parse_response(body: &str) -> Result<String, String> {
    let root: serde_json::Value = serde_json::from_str(body).map_err(|_| {
        tr!(
            "百度图片翻译返回了无法解析的结果",
            "Baidu image translation returned an unreadable response"
        )
        .to_owned()
    })?;
    let field = |value: &serde_json::Value, names: &[&str]| {
        names.iter().find_map(|name| match value.get(*name)? {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
    };
    let code = field(&root, &["error_code", "errorCode"]).unwrap_or_default();
    if !(code.is_empty() || code == "0" || code == "52000") {
        let message = field(&root, &["error_msg", "errorMsg"]).unwrap_or_default();
        return Err(describe_error(&code, &message));
    }
    let data = root
        .get("data")
        .filter(|data| data.is_object())
        .unwrap_or(&root);
    field(data, &["paste_img", "pasteImg"])
        .or_else(|| field(&root, &["paste_img", "pasteImg"]))
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| {
            tr!(
                "百度没有返回贴合图；请确认已开通「图片翻译 V2.0」",
                "Baidu returned no rendered image; make sure Picture Translation V2.0 is enabled"
            )
            .to_owned()
        })
}

fn describe_error(code: &str, message: &str) -> String {
    let mapped = match code {
        "54000" => tr!(
            "必填参数为空，请检查 APP ID",
            "A required parameter is empty; check the APP ID"
        ),
        "54001" | "55002" => tr!(
            "API Key 无效，请在开放平台「API Key 管理」创建",
            "Invalid API Key; create one under API Key management"
        ),
        "54004" => tr!(
            "百度翻译账户余额不足",
            "Insufficient Baidu Translate balance"
        ),
        "55001" => tr!(
            "请求参数不合法（语言或模式）",
            "Invalid request parameters (language or mode)"
        ),
        "55003" => tr!(
            "无法解析图片（可能过糊或排版过复杂）",
            "Cannot parse the image (blurry or too complex)"
        ),
        "55004" => tr!("贴合渲染失败", "Rendering the translated image failed"),
        "55005" => tr!("图片超过大小上限", "The image exceeds the size limit"),
        "55006" => tr!(
            "未开通或已停用「图片翻译 V2.0」",
            "Picture Translation V2.0 is not enabled"
        ),
        "59004" => tr!(
            "请求过于频繁，请稍后再试",
            "Too many requests; try again later"
        ),
        _ => "",
    };
    let detail = match (mapped.is_empty(), message.trim().is_empty()) {
        (true, true) => trf!("错误码 {}", "error code {}", code),
        (true, false) => message.trim().to_owned(),
        (false, true) => mapped.to_owned(),
        (false, false) => format!("{mapped}：{}", message.trim()),
    };
    trf!(
        "百度图片翻译失败：{}",
        "Baidu image translation failed: {}",
        detail
    )
}

fn encode_upload(image: &Image) -> Result<Vec<u8>, String> {
    let (width, height) = (image.width.max(1), image.height.max(1));
    if image.bgra.len() != width as usize * height as usize * 4 {
        return Err(tr!("没有可翻译的图片", "No image to translate").into());
    }
    let rgb: Vec<u8> = image
        .bgra
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0]])
        .collect();
    let mut buffer = image::RgbImage::from_raw(width, height, rgb)
        .ok_or_else(|| tr!("没有可翻译的图片", "No image to translate").to_owned())?;
    let mut scale = 1f32;
    if width.min(height) < MIN_SHORT_EDGE {
        scale = MIN_SHORT_EDGE as f32 / width.min(height) as f32;
    }
    if (width.max(height) as f32 * scale) > MAX_LONG_EDGE as f32 {
        scale = MAX_LONG_EDGE as f32 / width.max(height) as f32;
    }
    for quality in [90u8, 80, 70] {
        if scale != 1. {
            let (w, h) = (
                ((buffer.width() as f32 * scale).round() as u32).max(1),
                ((buffer.height() as f32 * scale).round() as u32).max(1),
            );
            buffer = image::imageops::resize(&buffer, w, h, image::imageops::FilterType::Triangle);
        }
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode_image(&buffer)
            .map_err(|error| error.to_string())?;
        if bytes.len() <= MAX_BYTES {
            return Ok(bytes);
        }
        scale = 0.75;
    }
    Err(tr!(
        "图片过大，请缩小选区后重试",
        "The image is too large; select a smaller area"
    )
    .into())
}

/// 贴合图缩放回原选区尺寸，保证与截图坐标一一对应。
fn decode_pasted(base64: &str, width: u32, height: u32) -> Result<Image, String> {
    let payload = base64.trim();
    let payload = match (payload.starts_with("data:"), payload.find(',')) {
        (true, Some(comma)) => &payload[comma + 1..],
        _ => payload,
    };
    let invalid = || {
        tr!(
            "百度返回的贴合图无法解码",
            "Cannot decode the image returned by Baidu"
        )
        .to_owned()
    };
    let bytes = base64_decode(payload).ok_or_else(invalid)?;
    let mut rgba = image::load_from_memory(&bytes)
        .map_err(|_| invalid())?
        .to_rgba8();
    if rgba.dimensions() != (width, height) {
        rgba = image::imageops::resize(&rgba, width, height, image::imageops::FilterType::Triangle);
    }
    let bgra = rgba
        .pixels()
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect();
    Ok(Image {
        width,
        height,
        bgra,
        readback_ms: 0.,
    })
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        };
        acc = acc << 6 | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_all_tail_lengths() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            let encoded = base64_encode(data);
            assert_eq!(base64_decode(&encoded).unwrap(), data);
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
    }

    #[test]
    fn engine_uses_baidu_only_when_explicitly_selected_with_credentials() {
        let mut preferences = crate::preferences::Preferences::default();
        assert_eq!(engine(&preferences), Engine::Local);
        preferences.baidu_app_id = "id".into();
        assert_eq!(engine(&preferences), Engine::Local);
        preferences.baidu_api_key = " key ".into();
        assert_eq!(engine(&preferences), Engine::Local);
        preferences.image_translate_engine = "baidu".into();
        assert!(matches!(engine(&preferences), Engine::Baidu(c) if c.api_key == "key"));
        preferences.image_translate_engine = "local".into();
        assert_eq!(engine(&preferences), Engine::Local);
    }

    #[test]
    fn response_errors_and_paste_image_are_parsed() {
        assert!(parse_response(r#"{"error_code":"55006","error_msg":"x"}"#).is_err());
        assert_eq!(
            parse_response(r#"{"error_code":"0","data":{"paste_img":"QUJD"}}"#).unwrap(),
            "QUJD"
        );
        assert!(parse_response(r#"{"error_code":0,"data":{}}"#).is_err());
        assert_eq!(target_code("ja"), "jp");
        assert_eq!(target_code("zh-CN"), "zh");
    }

    #[test]
    fn upload_encodes_jpeg_and_decode_restores_size() {
        let image = Image {
            width: 8,
            height: 4,
            bgra: vec![200; 8 * 4 * 4],
            readback_ms: 0.,
        };
        let jpeg = encode_upload(&image).unwrap();
        let decoded = decode_pasted(&base64_encode(&jpeg), 8, 4).unwrap();
        assert_eq!((decoded.width, decoded.height), (8, 4));
        assert_eq!(decoded.bgra.len(), 8 * 4 * 4);
    }
}
