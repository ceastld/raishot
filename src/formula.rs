//! 用户显式请求的公式识别；只上传本次传入的选区副本，不改变截图或导出像素。
use crate::{capture::Image, recognition::task::TaskToken, translate::Service, Result};
use serde_json::{json, Value};
use std::io::Read;

const MAX_SOURCE_PIXELS: u64 = 16_000_000;
const MAX_UPLOAD_PIXELS: u64 = 1_600_000;
const MAX_UPLOAD_SIDE: u32 = 2048;
const MAX_UPLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_LATEX_BYTES: usize = 64 * 1024;
const MAX_LATEX_CHARS: usize = 16_000;
const MAX_GROUP_DEPTH: usize = 64;

const PROMPT: &str = "只转写图像中可见的文字、数学公式和表格，按原有阅读顺序输出Markdown，不加解释或包裹全文的代码围栏。普通文字保持原文和段落；行内公式用\\( ... \\)，独立公式用\\[ ... \\]，内部使用可直接排版的LaTeX；表格使用Markdown表格。保留全部符号、上下标、分数、根号、矩阵各格、多行顺序和可见样式；保留中文。不得求解、化简、纠错、补全或翻译，不得依据常见公式猜测模糊内容。标签必须直接附着于对应的可见数学对象，不要用空白hspace作为underbrace/overbrace的基底。无法辨认的单个数学字符用\\text{?}占位。";

#[derive(Clone, Debug)]
pub(crate) struct FormulaResult {
    /// API content 原文，包括混合文字、公式、表格和不完整内容。
    pub(crate) raw_output: String,
    /// 兼容字段：供 Markdown 组件编辑、解析的源文，只剥完整全文代码围栏。
    /// 每个数学块单独 normalize_latex；不可对整份混合源文做数学校验。
    pub(crate) latex: String,
    /// 生成层诊断；局部数学校验由组件逐块完成，不阻断其它文字或表格。
    pub(crate) validation_error: Option<String>,
}

/// 阻塞操作，调用方必须在后台线程执行；取消保持 OCR 任务的类型身份。
pub(crate) fn recognize(
    service: &Service,
    image: &Image,
    task: &TaskToken,
) -> Result<FormulaResult> {
    task.checkpoint()?;
    let png = encode_upload(image, task)?;
    task.checkpoint()?;
    let body = request_body(service, &png);
    task.checkpoint()?;
    let response = crate::translate::post_json(service, &body)?;
    task.checkpoint()?;
    let streaming = response.content_type().contains("event-stream");
    let bytes = read_bounded(response.into_reader(), task)?;
    task.checkpoint()?;
    let completion = parse_response(&bytes, streaming)?;
    let result = result_from_completion(completion);
    task.checkpoint()?;
    Ok(result)
}

fn request_body(service: &Service, png: &[u8]) -> Value {
    let deepseek = service.is_deepseek();
    let mut body = json!({
        "model": service.model,
        "temperature": 0,
        "max_tokens": 4096,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": PROMPT},
            {"type": "image_url", "image_url": {
                "url": format!("data:image/png;base64,{}", base64(png)),
                "detail": if deepseek { "original" } else { "high" }
            }}
        ]}]
    });
    if deepseek {
        body["thinking"] = json!({"type": "disabled"});
    }
    body
}

fn encode_upload(image: &Image, task: &TaskToken) -> Result<Vec<u8>> {
    let pixels = u64::from(image.width) * u64::from(image.height);
    if pixels == 0
        || pixels > MAX_SOURCE_PIXELS
        || pixels.checked_mul(4) != Some(image.bgra.len() as u64)
    {
        return Err(tr!(
            "公式选区无效或过大，请缩小选区后重试",
            "The formula area is invalid or too large; select a smaller area"
        )
        .into());
    }
    let mut rgb = Vec::with_capacity(pixels as usize * 3);
    let mut luminance = 0u64;
    for (index, pixel) in image.bgra.chunks_exact(4).enumerate() {
        if index % 4096 == 0 {
            task.checkpoint()?;
        }
        let alpha = u32::from(pixel[3]);
        let composite =
            |value: u8| ((u32::from(value) * alpha + 255 * (255 - alpha) + 127) / 255) as u8;
        let (red, green, blue) = (
            composite(pixel[2]),
            composite(pixel[1]),
            composite(pixel[0]),
        );
        luminance += (u64::from(red) * 299 + u64::from(green) * 587 + u64::from(blue) * 114) / 1000;
        rgb.extend_from_slice(&[red, green, blue]);
    }
    // 这一步只作用于上传副本，透明像素先与白底合成，再统一深色极性。
    if luminance < pixels * 127 {
        rgb.iter_mut().for_each(|value| *value = 255 - *value);
    }
    task.checkpoint()?;
    let mut buffer = image::RgbImage::from_raw(image.width, image.height, rgb)
        .ok_or_else(|| tr!("公式图片无效", "Invalid formula image").to_owned())?;
    let scale = (MAX_UPLOAD_SIDE as f64 / f64::from(image.width.max(image.height)))
        .min((MAX_UPLOAD_PIXELS as f64 / pixels as f64).sqrt())
        .min(1.0);
    if scale < 1.0 {
        let width = (f64::from(image.width) * scale).floor().max(1.0) as u32;
        let height = (f64::from(image.height) * scale).floor().max(1.0) as u32;
        buffer = image::imageops::resize(
            &buffer,
            width,
            height,
            image::imageops::FilterType::Triangle,
        );
    }
    task.checkpoint()?;
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, buffer.width(), buffer.height());
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|_| tr!("公式图片编码失败", "Failed to encode the formula image"))?;
        writer
            .write_image_data(buffer.as_raw())
            .map_err(|_| tr!("公式图片编码失败", "Failed to encode the formula image"))?;
    }
    task.checkpoint()?;
    if bytes.len() > MAX_UPLOAD_BYTES {
        return Err(tr!(
            "公式图片过大，请缩小选区",
            "The formula image is too large; select a smaller area"
        )
        .into());
    }
    Ok(bytes)
}

fn read_bounded(mut reader: impl Read, task: &TaskToken) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        task.checkpoint()?;
        let read = reader.read(&mut chunk).map_err(|_| {
            tr!(
                "读取公式结果失败，请检查网络后重试",
                "Failed to read the formula result; check the connection and retry"
            )
        })?;
        task.checkpoint()?;
        if read == 0 {
            return Ok(bytes);
        }
        if bytes.len().saturating_add(read) > MAX_RESPONSE_BYTES {
            return Err(tr!(
                "公式服务返回的数据过大",
                "The formula response is too large"
            )
            .into());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
}

struct Completion {
    text: String,
    finish_reason: Option<String>,
    incomplete_stream: bool,
}

fn parse_response(bytes: &[u8], streaming: bool) -> Result<Completion> {
    let invalid = || {
        tr!(
            "公式服务返回的数据无效",
            "The formula service returned invalid data"
        )
        .to_owned()
    };
    let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    if !streaming {
        let value: Value = serde_json::from_str(text).map_err(|_| invalid())?;
        let choice = value.pointer("/choices/0").ok_or_else(invalid)?;
        let content = choice
            .pointer("/message/content")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        return Ok(Completion {
            text: content.to_owned(),
            finish_reason: choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            incomplete_stream: false,
        });
    }
    let mut completion = Completion {
        text: String::new(),
        finish_reason: None,
        incomplete_stream: false,
    };
    let mut event = Vec::new();
    let mut ended = false;
    for line in text.lines().chain(std::iter::once("")) {
        if let Some(data) = line.strip_prefix("data:") {
            event.push(data.trim_start());
        } else if line.trim().is_empty() && !event.is_empty() {
            let data = event.join("\n");
            event.clear();
            if data == "[DONE]" {
                ended = true;
                break;
            }
            let value: Value = serde_json::from_str(&data).map_err(|_| invalid())?;
            if value.get("error").is_some() {
                return Err(tr!(
                    "公式服务未能完成请求，请稍后重试",
                    "The formula service could not complete the request; try again later"
                )
                .into());
            }
            if let Some(choice) = value.pointer("/choices/0") {
                if let Some(content) = choice.pointer("/delta/content").and_then(Value::as_str) {
                    completion.text.push_str(content);
                }
                if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                    completion.finish_reason = Some(reason.to_owned());
                }
            }
        }
    }
    completion.incomplete_stream = !ended && completion.finish_reason.is_none();
    Ok(completion)
}

fn result_from_completion(completion: Completion) -> FormulaResult {
    let raw_output = completion.text;
    let latex = markdown_source(&raw_output);
    let mut validation_error =
        if raw_output.len() > MAX_LATEX_BYTES || raw_output.chars().count() > MAX_LATEX_CHARS {
            Some(
                tr!(
                "识别结果过长，已保留完整源文，请分段校对",
                "The result is too long; the full source is preserved for review in smaller parts"
            )
                .to_owned(),
            )
        } else if latex.trim().is_empty() {
            Some(
                tr!(
                    "未识别到内容，请检查选区后重试",
                    "No content was recognized; check the selection and retry"
                )
                .to_owned(),
            )
        } else {
            None
        };
    if completion.incomplete_stream || completion.finish_reason.as_deref() != Some("stop") {
        validation_error = Some(if completion.finish_reason.as_deref() == Some("length") {
            tr!(
                "公式输出已截断，请缩小选区或补全后校对",
                "The formula was truncated; select a smaller area or complete and review it"
            )
            .to_owned()
        } else {
            tr!(
                "公式结果未正常结束，请校对或重试",
                "The formula response did not finish normally; review or retry"
            )
            .to_owned()
        });
    }
    FormulaResult {
        raw_output,
        latex,
        validation_error,
    }
}

// 不剥数学定界符：它们决定混合 Markdown 中各段的类型。围栏不完整时保留原文。
fn markdown_source(raw: &str) -> String {
    let text = raw.trim().trim_start_matches('\u{feff}').trim();
    let Some(after_open) = text.strip_prefix("```") else {
        return raw.to_owned();
    };
    let Some(newline) = after_open.find('\n') else {
        return raw.to_owned();
    };
    let language = after_open[..newline].trim().to_ascii_lowercase();
    if !matches!(
        language.as_str(),
        "" | "tex" | "latex" | "math" | "markdown" | "md"
    ) {
        return raw.to_owned();
    }
    let content = &after_open[newline + 1..];
    let body = if content == "```" {
        ""
    } else if let Some(body) = content.strip_suffix("\n```") {
        body.strip_suffix('\r').unwrap_or(body)
    } else {
        return raw.to_owned();
    };
    // 不能把包含多个代码块的正常文档误当成一个包裹全文的围栏。
    if body.contains("```") {
        return raw.to_owned();
    }
    body.to_owned()
}

/// 校验单个数学段，剥完整数学外层标记并包装顶层普通中文；不接收整份 Markdown。
pub(crate) fn normalize_latex(raw: &str) -> std::result::Result<String, String> {
    if raw.len() > MAX_LATEX_BYTES || raw.chars().count() > MAX_LATEX_CHARS {
        return Err(tr!(
            "公式过长，请分段识别",
            "The formula is too long; recognize smaller parts"
        )
        .into());
    }
    let stripped = strip_wrappers(raw)?;
    validate(&stripped)?;
    let normalized = wrap_top_level_chinese(&stripped);
    if normalized.len() > MAX_LATEX_BYTES || normalized.chars().count() > MAX_LATEX_CHARS {
        return Err(tr!(
            "公式过长，请分段识别",
            "The formula is too long; recognize smaller parts"
        )
        .into());
    }
    Ok(normalized)
}

fn strip_wrappers(raw: &str) -> std::result::Result<String, String> {
    let mut text = raw.trim().trim_start_matches('\u{feff}').trim();
    for _ in 0..8 {
        if text.starts_with("```") {
            let Some(newline) = text.find('\n') else {
                return Err(wrapper_error());
            };
            let language = text[3..newline].trim().to_ascii_lowercase();
            if !matches!(language.as_str(), "" | "tex" | "latex" | "math") || !text.ends_with("```")
            {
                return Err(wrapper_error());
            }
            let body = text[newline + 1..text.len() - 3].trim();
            if body.contains("```") {
                return Err(wrapper_error());
            }
            text = body;
            continue;
        }
        let pair = [("$$", "$$"), ("\\[", "\\]"), ("\\(", "\\)"), ("$", "$")]
            .into_iter()
            .find(|(open, close)| {
                text.starts_with(open)
                    && text.ends_with(close)
                    && text.len() >= open.len() + close.len()
            });
        if let Some((open, close)) = pair {
            let body = &text[open.len()..text.len() - close.len()];
            if contains_unescaped(body, close) {
                return Err(wrapper_error());
            }
            text = body.trim();
            continue;
        }
        break;
    }
    Ok(text.to_owned())
}

fn wrapper_error() -> String {
    tr!(
        "公式外围标记不完整或混有其它内容，请校对",
        "Formula delimiters are incomplete or mixed with other content; review the source"
    )
    .into()
}

fn escaped_at(text: &str, index: usize) -> bool {
    text.as_bytes()[..index]
        .iter()
        .rev()
        .take_while(|byte| **byte == b'\\')
        .count()
        % 2
        != 0
}

fn contains_unescaped(text: &str, token: &str) -> bool {
    text.match_indices(token)
        .any(|(index, _)| !escaped_at(text, index))
}

fn command(text: &str, index: usize) -> (&str, usize) {
    let start = index + 1;
    let mut end = start;
    for (offset, ch) in text[start..].char_indices() {
        if offset == 0 || ch.is_alphabetic() {
            if offset > 0
                && !text[start..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphabetic)
            {
                break;
            }
            end = start + offset + ch.len_utf8();
            if !ch.is_alphabetic() {
                break;
            }
        } else {
            break;
        }
    }
    (&text[start..end], end)
}

fn group_after(text: &str, index: usize) -> Option<(&str, usize)> {
    let start = index + text[index..].len() - text[index..].trim_start().len();
    if text.as_bytes().get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut index = start;
    while index < text.len() {
        let ch = text[index..].chars().next()?;
        if ch == '\\' {
            index = command(text, index).1;
            continue;
        }
        if ch == '{' {
            depth += 1;
        }
        if ch == '}' {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some((&text[start + 1..index], index + 1));
            }
        }
        index += ch.len_utf8();
    }
    None
}

fn blank_math(text: &str) -> bool {
    let mut index = 0;
    while index < text.len() {
        let ch = text[index..].chars().next().unwrap();
        if ch.is_whitespace() || matches!(ch, '{' | '}' | '~') {
            index += ch.len_utf8();
            continue;
        }
        if ch != '\\' {
            return false;
        }
        let (name, end) = command(text, index);
        match name {
            "," | ";" | ":" | "!" | " " | "quad" | "qquad" | "enspace" | "hfill" => index = end,
            "hspace" | "hphantom" | "vphantom" | "phantom" => {
                let end = if text.as_bytes().get(end) == Some(&b'*') {
                    end + 1
                } else {
                    end
                };
                let Some((_, after)) = group_after(text, end) else {
                    return false;
                };
                index = after;
            }
            _ => return false,
        }
    }
    true
}

fn validate(text: &str) -> std::result::Result<(), String> {
    let invalid = || {
        tr!(
            "公式括号或环境不完整，请校对LaTeX",
            "Formula braces or environments are incomplete; review the LaTeX"
        )
        .to_owned()
    };
    if text.is_empty() {
        return Err(tr!("未识别到公式", "No formula was recognized").into());
    }
    let mut groups = 0usize;
    let mut left_right = 0usize;
    let mut environments: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < text.len() {
        let ch = text[index..].chars().next().unwrap();
        if ch.is_control() && !matches!(ch, '\n' | '\r' | '\t') {
            return Err(invalid());
        }
        if ch == '\\' {
            let (name, end) = command(text, index);
            if name.is_empty() {
                return Err(invalid());
            }
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("html")
                || lower.starts_with("pdf")
                || matches!(
                    lower.as_str(),
                    "input"
                        | "include"
                        | "includegraphics"
                        | "usepackage"
                        | "documentclass"
                        | "require"
                        | "write"
                        | "write18"
                        | "openout"
                        | "closeout"
                        | "openin"
                        | "read"
                        | "special"
                        | "directlua"
                        | "luaexec"
                        | "immediate"
                        | "catcode"
                        | "csname"
                        | "endcsname"
                        | "def"
                        | "gdef"
                        | "edef"
                        | "xdef"
                        | "let"
                        | "futurelet"
                        | "newcommand"
                        | "renewcommand"
                        | "providecommand"
                        | "newenvironment"
                        | "renewenvironment"
                        | "href"
                        | "url"
                        | "hyperlink"
                        | "hypertarget"
                        | "class"
                        | "style"
                        | "loop"
                        | "repeat"
                        | "everymath"
                        | "everydisplay"
                        | "expandafter"
                        | "scantokens"
                )
            {
                return Err(tr!(
                    "公式包含非数学指令，请删除后重试",
                    "The formula contains non-mathematical commands; remove them and retry"
                )
                .into());
            }
            match name {
                "[" | "]" | "(" | ")" => return Err(wrapper_error()),
                "left" => left_right += 1,
                "right" => left_right = left_right.checked_sub(1).ok_or_else(invalid)?,
                "middle" if left_right == 0 => return Err(invalid()),
                "begin" | "end" => {
                    let (environment, _) = group_after(text, end).ok_or_else(invalid)?;
                    if !math_environment(environment) {
                        return Err(tr!("只支持数学公式环境，请移除文档或其它内容", "Only math environments are supported; remove document or other content").into());
                    }
                    if name == "begin" {
                        environments.push(environment);
                    } else if environments.pop() != Some(environment) {
                        return Err(invalid());
                    }
                }
                "underbrace" | "overbrace" => {
                    if let Some((base, _)) = group_after(text, end) {
                        if blank_math(base) {
                            return Err(tr!("括号标签附在空白占位上，无法确认对应对象，请对照原图校正", "A brace label is attached to empty spacing; check its association against the image").into());
                        }
                    }
                }
                _ => {}
            }
            index = end;
            continue;
        }
        match ch {
            '{' => {
                groups += 1;
                if groups > MAX_GROUP_DEPTH {
                    return Err(invalid());
                }
            }
            '}' => groups = groups.checked_sub(1).ok_or_else(invalid)?,
            '$' | '`' | '%' => {
                return Err(tr!(
                    "公式中存在未转义的外围标记，请校对",
                    "The formula contains unescaped delimiters; review the source"
                )
                .into())
            }
            _ => {}
        }
        if left_right > MAX_GROUP_DEPTH || environments.len() > MAX_GROUP_DEPTH {
            return Err(invalid());
        }
        index += ch.len_utf8();
    }
    if groups != 0 || left_right != 0 || !environments.is_empty() {
        return Err(invalid());
    }
    Ok(())
}

fn math_environment(name: &str) -> bool {
    matches!(
        name,
        "aligned"
            | "alignedat"
            | "align"
            | "align*"
            | "alignat"
            | "alignat*"
            | "gather"
            | "gather*"
            | "gathered"
            | "split"
            | "matrix"
            | "pmatrix"
            | "bmatrix"
            | "Bmatrix"
            | "vmatrix"
            | "Vmatrix"
            | "smallmatrix"
            | "array"
            | "subarray"
            | "cases"
            | "dcases"
            | "rcases"
            | "equation"
            | "equation*"
            | "multline"
            | "multline*"
            | "flalign"
            | "flalign*"
    )
}

pub(crate) fn is_cjk(ch: char) -> bool {
    matches!(ch as u32, 0x3000..=0x303f | 0x3400..=0x4dbf | 0x4e00..=0x9fff |
        0xf900..=0xfaff | 0xff01..=0xff60 | 0x20000..=0x323af)
}

fn wrap_top_level_chinese(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut groups = 0usize;
    let mut index = 0;
    while index < text.len() {
        let ch = text[index..].chars().next().unwrap();
        if ch == '\\' {
            let end = command(text, index).1;
            output.push_str(&text[index..end]);
            index = end;
            continue;
        }
        if groups == 0 && is_cjk(ch) {
            let start = index;
            while index < text.len() && text[index..].chars().next().is_some_and(is_cjk) {
                index += text[index..].chars().next().unwrap().len_utf8();
            }
            output.push_str("\\text{");
            output.push_str(&text[start..index]);
            output.push('}');
            continue;
        }
        if ch == '{' {
            groups += 1;
        }
        if ch == '}' {
            groups = groups.saturating_sub(1);
        }
        output.push(ch);
        index += ch.len_utf8();
    }
    output
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            output.push(if index <= chunk.len() {
                ALPHABET[((value >> (18 - 6 * index)) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    output
}

#[cfg(test)]
#[path = "formula_tests.rs"]
mod tests;
