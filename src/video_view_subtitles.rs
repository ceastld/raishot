//! 显式本地字幕的有界读取、纯文本解析与媒体时间查询；不搜索相邻文件。
use std::{
    borrow::Cow,
    fmt,
    fs::File,
    io::{self, Read},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub(crate) const MAX_SUBTITLE_BYTES: usize = 4 * 1024 * 1024;
const MAX_SUBTITLE_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_SUBTITLE_LINES: usize = 100_000;
const MAX_LINE_BYTES: usize = 16 * 1024;
const MAX_SUBTITLE_CUES: usize = 20_000;
const MAX_CUE_TEXT_BYTES: usize = 32 * 1024;
const MAX_CUE_LINES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubtitleFormat {
    Srt,
    WebVtt,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SubtitleError {
    Canceled,
    UnsupportedEncoding,
    LimitExceeded(&'static str),
    Invalid { line: usize, detail: &'static str },
    NoCues,
    Load(String),
}

impl fmt::Display for SubtitleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Canceled => tr!("已取消字幕载入", "Subtitle loading canceled").to_string(),
            Self::UnsupportedEncoding => tr!(
                "字幕必须使用有效的 UTF-8 或带 BOM 的 UTF-16 编码，且不能包含空字符",
                "Subtitles require valid UTF-8 or BOM-marked UTF-16, without NUL characters"
            )
            .to_string(),
            Self::LimitExceeded(limit) => trf!(
                "字幕超过读取限制：{limit}",
                "Subtitle limit exceeded: {limit}"
            ),
            Self::Invalid { line, detail } => trf!(
                "字幕第 {line} 行无效：{detail}",
                "Invalid subtitle at line {line}: {detail}"
            ),
            Self::NoCues => {
                tr!("字幕中没有可显示的文本", "No subtitle text to display").to_string()
            }
            Self::Load(error) => trf!("无法载入字幕：{error}", "Cannot load subtitles: {error}"),
        };
        f.write_str(&message)
    }
}

impl std::error::Error for SubtitleError {}

pub(crate) type SubtitleResult<T> = Result<T, SubtitleError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubtitleCue {
    pub(crate) start: Duration,
    pub(crate) end: Duration,
    /// 只能按普通文字绘制；字幕位置、样式与其它标签不进入渲染接口。
    pub(crate) text: String,
}

#[derive(Debug)]
pub(crate) struct SubtitleTrack {
    format: SubtitleFormat,
    cues: Vec<SubtitleCue>,
    prefix_end: Vec<Duration>,
}

impl SubtitleTrack {
    /// 文件只由用户显式选择；每次重开前检查本地文件及祖先属性，再有界读取。
    /// 包含文件操作，应在后台调用；回到界面后仍须确认来源和请求身份。
    pub(crate) fn load_explicit(path: &Path, cancel: &AtomicBool) -> SubtitleResult<Self> {
        check_cancel(cancel)?;
        let path =
            std::path::absolute(path).map_err(|error| SubtitleError::Load(error.to_string()))?;
        crate::image_collection::ensure_local_file(&path).map_err(SubtitleError::Load)?;
        check_cancel(cancel)?;
        let file = File::open(&path).map_err(|error| SubtitleError::Load(error.to_string()))?;
        let bytes = read_bounded(file, cancel)?;
        Self::parse(&bytes, cancel)
    }

    /// 按内容识别 SRT/WebVTT；接受UTF-8及带BOM的UTF-16LE/BE，保留CRLF处理。
    pub(crate) fn parse(bytes: &[u8], cancel: &AtomicBool) -> SubtitleResult<Self> {
        let source = decode_text(bytes, cancel)?;
        let mut lines = Vec::new();
        for line in source.split('\n') {
            check_cancel(cancel)?;
            if lines.len() == MAX_SUBTITLE_LINES {
                return Err(SubtitleError::LimitExceeded(tr!("总行数", "line count")));
            }
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.len() > MAX_LINE_BYTES {
                return Err(SubtitleError::LimitExceeded(tr!("单行长度", "line length")));
            }
            lines.push(line);
        }
        let Some(first) = lines.iter().position(|line| !line.trim().is_empty()) else {
            return Err(SubtitleError::NoCues);
        };
        let header = lines[first].trim();
        let format = if is_word_marker(header, "WEBVTT") {
            SubtitleFormat::WebVtt
        } else {
            SubtitleFormat::Srt
        };
        let mut cursor = first;
        if format == SubtitleFormat::WebVtt {
            while cursor < lines.len() && !lines[cursor].trim().is_empty() {
                check_cancel(cancel)?;
                let line = lines[cursor];
                reject_timestamp_map(line, cursor + 1)?;
                if line.contains("-->") {
                    return Err(invalid(
                        cursor + 1,
                        tr!(
                            "字幕头后缺少空行",
                            "Missing blank line after the subtitle header"
                        ),
                    ));
                }
                cursor += 1;
            }
        }
        let mut cues = Vec::new();
        let mut cue_count = 0;
        while cursor < lines.len() {
            check_cancel(cancel)?;
            if lines[cursor].trim().is_empty() {
                cursor += 1;
                continue;
            }
            let start = cursor;
            while cursor < lines.len() && !lines[cursor].trim().is_empty() {
                check_cancel(cancel)?;
                cursor += 1;
            }
            let block = &lines[start..cursor];
            let marker = block[0].trim();
            if format == SubtitleFormat::WebVtt {
                if is_word_marker(marker, "NOTE") || matches!(marker, "STYLE" | "REGION") {
                    continue;
                }
            }
            cue_count += 1;
            if cue_count > MAX_SUBTITLE_CUES {
                return Err(SubtitleError::LimitExceeded(tr!("字幕条数", "cue count")));
            }
            if let Some(cue) = parse_cue(block, start + 1, format, cancel)? {
                cues.push(cue);
            }
        }
        if cues.is_empty() {
            return Err(SubtitleError::NoCues);
        }
        check_cancel(cancel)?;
        // 稳定排序：相同开始时间沿输入顺序显示，重叠字幕不丢失。
        cues.sort_by_key(|cue| cue.start);
        let mut latest_end = Duration::ZERO;
        let mut prefix_end = Vec::with_capacity(cues.len());
        for cue in &cues {
            check_cancel(cancel)?;
            latest_end = latest_end.max(cue.end);
            prefix_end.push(latest_end);
        }
        Ok(Self {
            format,
            cues,
            prefix_end,
        })
    }

    pub(crate) fn format(&self) -> SubtitleFormat {
        self.format
    }

    pub(crate) fn cues(&self) -> &[SubtitleCue] {
        &self.cues
    }

    /// 区间为 [start, end)，重叠项全部按开始时间及输入顺序返回。
    /// 无播放游标，向前/向后 seek 采用相同查询；不分配合并文本。
    /// 长时间重叠时仍可能扫描全部有界条目，界面应限制同时拼接的文本量。
    pub(crate) fn visible_at(&self, pts: Duration) -> impl Iterator<Item = &SubtitleCue> {
        let upper = self.cues.partition_point(|cue| cue.start <= pts);
        let lower = self.prefix_end[..upper].partition_point(|end| *end <= pts);
        self.cues[lower..upper]
            .iter()
            .filter(move |cue| pts < cue.end)
    }
}

fn decode_text<'a>(bytes: &'a [u8], cancel: &AtomicBool) -> SubtitleResult<Cow<'a, str>> {
    check_cancel(cancel)?;
    if bytes.len() > MAX_SUBTITLE_BYTES {
        return Err(byte_limit());
    }
    let utf16 = bytes
        .strip_prefix(&[0xff, 0xfe])
        .map(|body| (body, true))
        .or_else(|| bytes.strip_prefix(&[0xfe, 0xff]).map(|body| (body, false)));
    if let Some((body, little_endian)) = utf16 {
        if body.len() % 2 != 0 {
            return Err(SubtitleError::UnsupportedEncoding);
        }
        let units = body.chunks_exact(2).map(|pair| {
            let bytes = [pair[0], pair[1]];
            if little_endian {
                u16::from_le_bytes(bytes)
            } else {
                u16::from_be_bytes(bytes)
            }
        });
        return decode_utf16_text(units, cancel).map(Cow::Owned);
    }
    // 无BOM时仅接受UTF-8；UTF-16中的空字节不能作为正文绕过编码检查。
    let source = std::str::from_utf8(bytes).map_err(|_| SubtitleError::UnsupportedEncoding)?;
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    if source.len() > MAX_SUBTITLE_TEXT_BYTES {
        return Err(text_byte_limit());
    }
    for chunk in source.as_bytes().chunks(8192) {
        check_cancel(cancel)?;
        if chunk.contains(&0) {
            return Err(SubtitleError::UnsupportedEncoding);
        }
    }
    check_cancel(cancel)?;
    Ok(Cow::Borrowed(source))
}

fn decode_utf16_text(
    units: impl Iterator<Item = u16>,
    cancel: &AtomicBool,
) -> SubtitleResult<String> {
    check_cancel(cancel)?;
    // 每个UTF-16码元至多产生3个UTF-8字节；预分配也受文本预算约束。
    let capacity = units
        .size_hint()
        .1
        .unwrap_or(0)
        .saturating_mul(3)
        .min(MAX_SUBTITLE_TEXT_BYTES);
    let mut text = String::with_capacity(capacity);
    for (index, character) in char::decode_utf16(units).enumerate() {
        if index % 1024 == 0 {
            check_cancel(cancel)?;
        }
        let character = character.map_err(|_| SubtitleError::UnsupportedEncoding)?;
        if character == '\0' {
            return Err(SubtitleError::UnsupportedEncoding);
        }
        if text.len() + character.len_utf8() > MAX_SUBTITLE_TEXT_BYTES {
            return Err(text_byte_limit());
        }
        text.push(character);
    }
    check_cancel(cancel)?;
    Ok(text)
}

fn text_byte_limit() -> SubtitleError {
    SubtitleError::LimitExceeded(tr!("解码后文本字节数", "decoded text byte count"))
}

fn read_bounded(reader: impl Read, cancel: &AtomicBool) -> SubtitleResult<Vec<u8>> {
    let mut reader = reader.take((MAX_SUBTITLE_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        check_cancel(cancel)?;
        let count = match reader.read(&mut chunk) {
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(SubtitleError::Load(error.to_string())),
        };
        if count == 0 {
            break;
        }
        if bytes.len() + count > MAX_SUBTITLE_BYTES {
            return Err(byte_limit());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    check_cancel(cancel)?;
    Ok(bytes)
}

fn check_cancel(cancel: &AtomicBool) -> SubtitleResult<()> {
    if cancel.load(Ordering::Acquire) {
        Err(SubtitleError::Canceled)
    } else {
        Ok(())
    }
}

fn byte_limit() -> SubtitleError {
    SubtitleError::LimitExceeded(tr!("文件字节数", "file byte count"))
}

fn invalid(line: usize, detail: &'static str) -> SubtitleError {
    SubtitleError::Invalid { line, detail }
}

fn is_word_marker(line: &str, word: &str) -> bool {
    line == word
        || line
            .strip_prefix(word)
            .is_some_and(|rest| rest.starts_with(' ') || rest.starts_with('\t'))
}

fn reject_timestamp_map(line: &str, number: usize) -> SubtitleResult<()> {
    if line
        .trim_start()
        .strip_prefix("X-TIMESTAMP-MAP")
        .is_some_and(|rest| rest.trim_start().starts_with('='))
    {
        return Err(invalid(
            number,
            tr!(
                "暂不支持带外部时间映射的字幕",
                "External subtitle timestamp mapping is not supported"
            ),
        ));
    }
    Ok(())
}

fn parse_cue(
    block: &[&str],
    line: usize,
    format: SubtitleFormat,
    cancel: &AtomicBool,
) -> SubtitleResult<Option<SubtitleCue>> {
    let timing = if block[0].contains("-->") { 0 } else { 1 };
    if timing == 1
        && format == SubtitleFormat::Srt
        && !block[0].trim().bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid(line, tr!("字幕序号无效", "Invalid subtitle index")));
    }
    let timing_line = block
        .get(timing)
        .ok_or_else(|| invalid(line, tr!("缺少字幕时间段", "Missing subtitle time range")))?;
    let (start, end) = parse_timing(timing_line, format).ok_or_else(|| {
        invalid(
            line + timing,
            tr!(
                "时间段无效或结束不晚于开始",
                "Invalid time range or end is not after start"
            ),
        )
    })?;
    let body = &block[timing + 1..];
    if body.len() > MAX_CUE_LINES {
        return Err(SubtitleError::LimitExceeded(tr!(
            "单条字幕行数",
            "lines per cue"
        )));
    }
    let mut text = String::new();
    for (index, source) in body.iter().enumerate() {
        check_cancel(cancel)?;
        if source.chars().any(|ch| ch.is_control() && ch != '\t') {
            return Err(invalid(
                line + timing + 1 + index,
                tr!(
                    "字幕包含无效控制字符",
                    "Subtitle contains invalid control characters"
                ),
            ));
        }
        if index > 0 {
            text.push('\n');
        }
        text.push_str(source.trim());
        if text.len() > MAX_CUE_TEXT_BYTES {
            return Err(SubtitleError::LimitExceeded(tr!(
                "单条字幕文本",
                "cue text length"
            )));
        }
    }
    let text = plain_text(&text, format, cancel)?;
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if text.len() > MAX_CUE_TEXT_BYTES {
        return Err(SubtitleError::LimitExceeded(tr!(
            "单条字幕文本",
            "cue text length"
        )));
    }
    Ok(Some(SubtitleCue {
        start,
        end,
        text: text.to_string(),
    }))
}

fn parse_timing(line: &str, format: SubtitleFormat) -> Option<(Duration, Duration)> {
    let (start, rest) = line.split_once("-->")?;
    if rest.contains("-->") {
        return None;
    }
    let end = match format {
        SubtitleFormat::Srt => rest.trim(),
        // 位置、对齐和区域设置只忽略，不交给界面执行。
        SubtitleFormat::WebVtt => rest.split_whitespace().next()?,
    };
    let start = Duration::from_millis(timestamp_millis(start.trim(), format)?);
    let end = Duration::from_millis(timestamp_millis(end, format)?);
    (start < end).then_some((start, end))
}

fn timestamp_millis(value: &str, format: SubtitleFormat) -> Option<u64> {
    let mut parts = value.split(':');
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let (hours, minutes, seconds) = match third {
        Some(seconds) => (digits(first)?, fixed_digits(second, 2)?, seconds),
        None if format == SubtitleFormat::WebVtt => (0, fixed_digits(first, 2)?, second),
        None => return None,
    };
    let separator = if format == SubtitleFormat::Srt {
        ','
    } else {
        '.'
    };
    let (seconds, millis) = seconds.split_once(separator)?;
    let seconds = fixed_digits(seconds, 2)?;
    let millis = fixed_digits(millis, 3)?;
    if minutes >= 60 || seconds >= 60 {
        return None;
    }
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000)?
        .checked_add(seconds * 1000)?
        .checked_add(millis)
}

fn digits(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn fixed_digits(value: &str, length: usize) -> Option<u64> {
    (value.len() == length).then(|| digits(value)).flatten()
}

fn plain_text(source: &str, format: SubtitleFormat, cancel: &AtomicBool) -> SubtitleResult<String> {
    let mut stripped = String::with_capacity(source.len());
    let mut literal = 0;
    let mut tag = None;
    // 单遍扫描；未闭合或未知标签按普通文字保留，不吞掉后面的正文。
    for (index, ch) in source.char_indices() {
        if index.is_multiple_of(1024) {
            check_cancel(cancel)?;
        }
        if ch == '<' {
            stripped.push_str(&source[literal..index]);
            tag = Some(index);
            literal = index;
        } else if ch == '>' {
            if let Some(start) = tag.take() {
                if !is_inline_tag(&source[start + 1..index], format) {
                    stripped.push_str(&source[start..index + 1]);
                }
                literal = index + 1;
            }
        }
    }
    stripped.push_str(&source[literal..]);
    let mut text = String::with_capacity(stripped.len());
    let mut cursor = 0;
    // 标签先处理，实体后解码；&lt;b&gt; 应保留为字面文字。
    while let Some(offset) = stripped[cursor..].find('&') {
        check_cancel(cancel)?;
        let start = cursor + offset;
        text.push_str(&stripped[cursor..start]);
        let entity = &stripped[start + 1..];
        let end = entity
            .as_bytes()
            .iter()
            .take(32)
            .position(|byte| *byte == b';');
        if let Some(end) = end {
            if let Some(ch) = decode_entity(&entity[..end]) {
                text.push(ch);
                cursor = start + end + 2;
                continue;
            }
        }
        text.push('&');
        cursor = start + 1;
    }
    text.push_str(&stripped[cursor..]);
    check_cancel(cancel)?;
    Ok(text.replace('\t', " "))
}

fn is_inline_tag(body: &str, format: SubtitleFormat) -> bool {
    let body = body.trim();
    let name = body
        .strip_prefix('/')
        .unwrap_or(body)
        .split(|ch: char| ch.is_ascii_whitespace() || ch == '.')
        .next()
        .unwrap_or("");
    matches!(
        name,
        "b" | "i" | "u" | "font" | "c" | "v" | "lang" | "ruby" | "rt"
    ) || (format == SubtitleFormat::WebVtt && timestamp_millis(body, format).is_some())
}

fn decode_entity(value: &str) -> Option<char> {
    Some(match value {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "lrm" => '\u{200e}',
        "rlm" => '\u{200f}',
        _ => {
            let number = value.strip_prefix('#')?;
            let parsed = if let Some(hex) = number
                .strip_prefix('x')
                .or_else(|| number.strip_prefix('X'))
            {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                if !number.bytes().all(|byte| byte.is_ascii_digit()) {
                    return None;
                }
                number.parse().ok()?
            };
            char::from_u32(parsed)
                .filter(|ch| !ch.is_control())
                .unwrap_or('\u{fffd}')
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> SubtitleResult<SubtitleTrack> {
        SubtitleTrack::parse(text.as_bytes(), &AtomicBool::new(false))
    }

    fn utf16_with_bom(text: &str, little_endian: bool) -> Vec<u8> {
        let mut bytes = if little_endian {
            vec![0xff, 0xfe]
        } else {
            vec![0xfe, 0xff]
        };
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            });
        }
        bytes
    }

    fn visible(track: &SubtitleTrack, millis: u64) -> Vec<&str> {
        track
            .visible_at(Duration::from_millis(millis))
            .map(|cue| cue.text.as_str())
            .collect()
    }

    #[test]
    fn utf8_bom_crlf_unicode_and_literal_entities_survive_as_plain_text() {
        let track = parse("\u{feff}1\r\n00:00:01,250 --> 00:00:03,500\r\n<b>中文 🎬</b> &amp; &lt;b&gt;\r\n第二行\r\n").unwrap();
        assert_eq!(track.format(), SubtitleFormat::Srt);
        assert_eq!(track.cues()[0].text, "中文 🎬 & <b>\n第二行");
        assert!(visible(&track, 1249).is_empty());
        assert_eq!(visible(&track, 1250), ["中文 🎬 & <b>\n第二行"]);
        assert!(visible(&track, 3500).is_empty());
    }

    #[test]
    fn utf16_bom_both_byte_orders_preserve_srt_and_vtt_unicode() {
        let cancel = AtomicBool::new(false);
        for little_endian in [true, false] {
            for (source, format) in [
                (
                    "1\r\n00:00:01,000 --> 00:00:02,000\r\nHello 中文 🎬\r\n",
                    SubtitleFormat::Srt,
                ),
                (
                    "WEBVTT\r\n\r\n00:01.000 --> 00:02.000\r\nHello 中文 🎬\r\n",
                    SubtitleFormat::WebVtt,
                ),
            ] {
                let bytes = utf16_with_bom(source, little_endian);
                let track = SubtitleTrack::parse(&bytes, &cancel).unwrap();
                assert_eq!(track.format(), format);
                assert_eq!(visible(&track, 1000), ["Hello 中文 🎬"]);
                assert!(visible(&track, 2000).is_empty());
                // 不带BOM时不会根据正文或空字节猜测字节序。
                assert!(matches!(
                    SubtitleTrack::parse(&bytes[2..], &cancel),
                    Err(SubtitleError::UnsupportedEncoding)
                ));
            }
        }
    }

    #[test]
    fn utf16_rejects_odd_bytes_unpaired_surrogates_and_nul() {
        let cancel = AtomicBool::new(false);
        for little_endian in [true, false] {
            let mut odd = utf16_with_bom("", little_endian);
            odd.push(b'x');
            assert!(matches!(
                SubtitleTrack::parse(&odd, &cancel),
                Err(SubtitleError::UnsupportedEncoding)
            ));
            for units in [vec![0xd800_u16], vec![0xdc00], vec![0xd800, 0x41], vec![0]] {
                let mut bytes = utf16_with_bom("", little_endian);
                for unit in units {
                    bytes.extend_from_slice(&if little_endian {
                        unit.to_le_bytes()
                    } else {
                        unit.to_be_bytes()
                    });
                }
                assert!(matches!(
                    SubtitleTrack::parse(&bytes, &cancel),
                    Err(SubtitleError::UnsupportedEncoding)
                ));
            }
        }
    }

    #[test]
    fn utf16_decoded_text_budget_and_cancellation_are_independent_of_input_size() {
        let cancel = AtomicBool::new(false);
        let text = "中".repeat(MAX_SUBTITLE_TEXT_BYTES / 3 + 1);
        let bytes = utf16_with_bom(&text, true);
        assert!(bytes.len() <= MAX_SUBTITLE_BYTES);
        assert_eq!(decode_text(&bytes, &cancel), Err(text_byte_limit()));

        cancel.store(true, Ordering::Release);
        for little_endian in [true, false] {
            let bytes = utf16_with_bom("00:00:00,000 --> 00:00:01,000\ntext", little_endian);
            assert!(matches!(
                SubtitleTrack::parse(&bytes, &cancel),
                Err(SubtitleError::Canceled)
            ));
        }
        cancel.store(false, Ordering::Release);
        let units = (0..2048).map(|index| {
            if index == 1024 {
                cancel.store(true, Ordering::Release);
            }
            u16::from(b'a')
        });
        assert_eq!(
            decode_utf16_text(units, &cancel),
            Err(SubtitleError::Canceled)
        );
    }

    #[test]
    fn vtt_skips_metadata_blocks_but_keeps_identifiers_and_ignores_cue_settings() {
        let track = parse("WEBVTT - 字幕\r\nKind: captions\r\n\r\nNOTE comment\r\nnot a cue\r\n\r\nSTYLE\r\n::cue { color: red }\r\n\r\nREGION\r\nid:place\r\n\r\nNOTEworthy\r\n00:01.000 --> 00:03.000 align:start position:20%\r\n<v 旁白><c.green>你好</c> <00:02.000>world</v>\r\n").unwrap();
        assert_eq!(track.format(), SubtitleFormat::WebVtt);
        assert_eq!(track.cues().len(), 1);
        assert_eq!(visible(&track, 2000), ["你好 world"]);
        let identifier_track =
            parse("WEBVTT\n\nX-TIMESTAMP-MAP-scene-1\n00:01.000 --> 00:02.000\ntext").unwrap();
        assert_eq!(visible(&identifier_track, 1000), ["text"]);
        assert!(parse(
            "WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00.000,MPEGTS:900000\n\n00:00.000 --> 00:01.000\na"
        )
        .is_err());
    }

    #[test]
    fn overlapping_cues_are_stable_and_seek_has_no_stale_cursor() {
        let track = parse("00:00:02,000 --> 00:00:04,000\nsecond\n\n00:00:01,000 --> 00:00:05,000\nfirst\n\n00:00:02,000 --> 00:00:03,000\nthird\n").unwrap();
        assert_eq!(visible(&track, 2500), ["first", "second", "third"]);
        assert_eq!(visible(&track, 3000), ["first", "second"]);
        assert_eq!(visible(&track, 4500), ["first"]);
        assert_eq!(visible(&track, 1000), ["first"]);
        assert!(visible(&track, 5000).is_empty());
        assert!(visible(&track, 0).is_empty());
    }

    #[test]
    fn malformed_times_ranges_and_encodings_fail_explicitly() {
        for timing in [
            "00:60:00,000 --> 01:00:01,000",
            "00:00:60,000 --> 00:01:01,000",
            "00:00:01,00 --> 00:00:02,000",
            "-1:00:01,000 --> 00:00:02,000",
            "00:00:02,000 --> 00:00:02,000",
            "00:00:03,000 --> 00:00:02,000",
            "18446744073709551615:00:00,000 --> 18446744073709551615:00:01,000",
        ] {
            assert!(parse(&format!("{timing}\ntext")).is_err(), "{timing}");
        }
        for bytes in [&b"\xff\xfe0"[..], &b"\xff"[..], &b"0\0:\0"[..]] {
            assert!(matches!(
                SubtitleTrack::parse(bytes, &AtomicBool::new(false)),
                Err(SubtitleError::UnsupportedEncoding)
            ));
        }
    }

    #[test]
    fn empty_payloads_are_skipped_and_empty_tracks_are_reported() {
        for text in [
            "",
            " \n\r\n",
            "WEBVTT\n\nNOTE only a comment\n",
            "00:00:00,000 --> 00:00:01,000\n<b></b>\n",
        ] {
            assert!(matches!(parse(text), Err(SubtitleError::NoCues)));
        }
        for bytes in [&b"\xff\xfe"[..], &b"\xfe\xff"[..]] {
            assert!(matches!(
                SubtitleTrack::parse(bytes, &AtomicBool::new(false)),
                Err(SubtitleError::NoCues)
            ));
        }
        let track =
            parse("WEBVTT\n\n00:00.000 --> 00:01.000\n\n00:01.000 --> 00:02.000\ntext\n").unwrap();
        assert_eq!(track.cues().len(), 1);
        assert_eq!(visible(&track, 1000), ["text"]);
    }

    #[test]
    fn byte_line_cue_and_text_limits_are_enforced_before_large_outputs() {
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            SubtitleTrack::parse(&vec![b'x'; MAX_SUBTITLE_BYTES + 1], &cancel),
            Err(SubtitleError::LimitExceeded(_))
        ));
        assert!(matches!(
            parse(&"\n".repeat(MAX_SUBTITLE_LINES)),
            Err(SubtitleError::LimitExceeded(_))
        ));
        assert!(matches!(
            parse(&"x".repeat(MAX_LINE_BYTES + 1)),
            Err(SubtitleError::LimitExceeded(_))
        ));
        let oversized_text = format!(
            "00:00:00,000 --> 00:00:01,000\n{}\n{}\n{}",
            "x".repeat(11_000),
            "x".repeat(11_000),
            "x".repeat(11_000)
        );
        assert!(matches!(
            parse(&oversized_text),
            Err(SubtitleError::LimitExceeded(_))
        ));
        let too_many_lines = format!(
            "00:00:00,000 --> 00:00:01,000\n{}",
            "x\n".repeat(MAX_CUE_LINES + 1)
        );
        assert!(matches!(
            parse(&too_many_lines),
            Err(SubtitleError::LimitExceeded(_))
        ));
        let too_many_cues = "00:00:00,000 --> 00:00:01,000\nx\n\n".repeat(MAX_SUBTITLE_CUES + 1);
        assert!(matches!(
            parse(&too_many_cues),
            Err(SubtitleError::LimitExceeded(_))
        ));
    }

    #[test]
    fn bounded_reader_stops_at_limit_and_checks_cancel_between_chunks() {
        struct Reader<'a> {
            read: usize,
            cancel: Option<&'a AtomicBool>,
        }
        impl Read for Reader<'_> {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                bytes.fill(b'x');
                self.read += bytes.len();
                if let Some(cancel) = self.cancel {
                    cancel.store(true, Ordering::Release);
                }
                Ok(bytes.len())
            }
        }
        let cancel = AtomicBool::new(false);
        let mut reader = Reader {
            read: 0,
            cancel: None,
        };
        assert!(matches!(
            read_bounded(&mut reader, &cancel),
            Err(SubtitleError::LimitExceeded(_))
        ));
        assert_eq!(reader.read, MAX_SUBTITLE_BYTES + 1);
        let mut reader = Reader {
            read: 0,
            cancel: Some(&cancel),
        };
        assert_eq!(
            read_bounded(&mut reader, &cancel),
            Err(SubtitleError::Canceled)
        );
        assert_eq!(reader.read, 8192);
        assert!(matches!(
            SubtitleTrack::parse(b"", &cancel),
            Err(SubtitleError::Canceled)
        ));
    }

    #[test]
    fn unclosed_tags_unknown_entities_and_numeric_controls_stay_safe_text() {
        let track = parse(
            "00:00:00,000 --> 00:00:01,000\n1 < 2 and <b>text</b> &unknown; &#x4F60; &#0; <open",
        )
        .unwrap();
        assert_eq!(visible(&track, 0), ["1 < 2 and text &unknown; 你 � <open"]);
    }
}
