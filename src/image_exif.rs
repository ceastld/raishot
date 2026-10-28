//! 只提取可独立搬移的拍摄字段；导出时重建目录，不携带原偏移、缩略图或私有块。
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_INPUT: usize = 256 * 1024;
const MAX_VALUES: usize = 32 * 1024;
const MAX_STRING: usize = 4 * 1024;
const MAX_TAGS: usize = 256;
const MAX_OUTPUT: usize = 65_527;
type CancelCheck<'a> = &'a dyn Fn() -> Result<(), String>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExifSummary {
    /// 只计从IFD0/ExifIFD实际保留的源标签，不含导出时生成的标签。
    pub retained_fields: u16,
    /// 只计这两个已扫描目录的未保留标签，不统计未遍历GPS/缩略图内的字段。
    pub dropped_fields: u16,
    /// 扫描目录中发现GPS指针；未遍历其内容。
    pub excluded_gps: bool,
    /// 发现缩略图目录指针或缩略图标签；未遍历其内容。
    pub excluded_thumbnail: bool,
    /// 发现MakerNote等已知私有块；未知标签另计入dropped_fields。
    pub excluded_private: bool,
}

#[derive(Clone, Debug)]
pub struct SupportedExif {
    ifd0: Vec<Field>,
    photo: Vec<Field>,
    summary: ExifSummary,
}

#[derive(Clone, Debug)]
struct Field {
    tag: u16,
    kind: u16,
    count: u32,
    // 数值统一为小端；文本保留已验证的ASCII和终止符。
    value: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Endian(bool);
impl Endian {
    fn u16(self, data: &[u8]) -> u16 {
        let bytes = data[..2].try_into().unwrap();
        if self.0 {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        }
    }
    fn u32(self, data: &[u8]) -> u32 {
        let bytes = data[..4].try_into().unwrap();
        if self.0 {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        }
    }
}

#[derive(Clone, Copy)]
struct RawField<'a> {
    tag: u16,
    kind: u16,
    count: u32,
    value: &'a [u8],
}

struct Directory<'a> {
    fields: Vec<RawField<'a>>,
    start: usize,
    end: usize,
    next: u32,
}

fn message(zh: &'static str, en: &'static str) -> String {
    crate::i18n::tr(zh, en).into()
}
fn invalid() -> String {
    message(
        "EXIF目录或数据范围无效",
        "Invalid EXIF directory or data range",
    )
}
fn allocation() -> String {
    message("EXIF字段分配失败", "EXIF field allocation failed")
}
fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        Err(message("EXIF处理已取消", "EXIF processing canceled"))
    } else {
        Ok(())
    }
}
fn reserved<T>(count: usize) -> Result<Vec<T>, String> {
    let mut result = Vec::new();
    result.try_reserve_exact(count).map_err(|_| allocation())?;
    Ok(result)
}
fn range(bytes: &[u8], start: usize, length: usize) -> Result<&[u8], String> {
    let end = start.checked_add(length).ok_or_else(invalid)?;
    bytes.get(start..end).ok_or_else(invalid)
}
fn type_width(kind: u16) -> Option<usize> {
    match kind {
        1 | 2 | 6 | 7 | 129 => Some(1),
        3 | 8 => Some(2),
        4 | 9 | 11 | 13 => Some(4),
        5 | 10 | 12 => Some(8),
        _ => None,
    }
}

fn directory<'a>(
    bytes: &'a [u8],
    start: u32,
    endian: Endian,
    cancel: CancelCheck<'_>,
) -> Result<Directory<'a>, String> {
    cancel()?;
    let start = start as usize;
    if start < 8 {
        return Err(invalid());
    }
    let count = usize::from(endian.u16(range(bytes, start, 2)?));
    if count > MAX_TAGS {
        return Err(message(
            "EXIF每目录最多256个标签",
            "EXIF directory exceeds 256 tags",
        ));
    }
    let size = count
        .checked_mul(12)
        .and_then(|size| size.checked_add(6))
        .ok_or_else(invalid)?;
    let data = range(bytes, start, size)?;
    let mut fields: Vec<RawField<'a>> = reserved(count)?;
    for row in data[2..size - 4].chunks_exact(12) {
        cancel()?;
        let tag = endian.u16(row);
        if fields.iter().any(|field| field.tag == tag) {
            return Err(message(
                "EXIF目录含重复标签",
                "Duplicate EXIF directory tag",
            ));
        }
        let kind = endian.u16(&row[2..]);
        let count = endian.u32(&row[4..]);
        let length = type_width(kind)
            .and_then(|width| (count as usize).checked_mul(width))
            .ok_or_else(invalid)?;
        let value = if length <= 4 {
            &row[8..8 + length]
        } else {
            let offset = endian.u32(&row[8..]) as usize;
            if offset < 8 {
                return Err(invalid());
            }
            range(bytes, offset, length)?
        };
        fields.push(RawField {
            tag,
            kind,
            count,
            value,
        });
    }
    Ok(Directory {
        fields,
        start,
        end: start + size,
        next: endian.u32(&data[size - 4..]),
    })
}

#[derive(Clone, Copy)]
enum Rule {
    Text,
    Copyright,
    Date,
    Offset,
    Subsecond,
    Short,
    Rational,
    SignedRational,
}
fn rule(photo: bool, tag: u16) -> Option<Rule> {
    if !photo {
        return match tag {
            0x010e | 0x010f | 0x0110 | 0x013b => Some(Rule::Text),
            0x8298 => Some(Rule::Copyright),
            _ => None,
        };
    }
    match tag {
        0x9003 => Some(Rule::Date),
        0x9011 => Some(Rule::Offset),
        0x9291 => Some(Rule::Subsecond),
        0xa433 | 0xa434 => Some(Rule::Text),
        0x8827 | 0x8822 | 0x9207 | 0x9209 => Some(Rule::Short),
        0x829a | 0x829d | 0x920a => Some(Rule::Rational),
        0x9204 => Some(Rule::SignedRational),
        _ => None,
    }
}
fn ascii(value: &[u8], copyright: bool) -> bool {
    if !(1..=MAX_STRING).contains(&value.len()) || value.last() != Some(&0) {
        return false;
    }
    let body = &value[..value.len() - 1];
    if copyright {
        body.iter().filter(|byte| **byte == 0).count() <= 1
            && body.iter().all(|byte| *byte == 0 || ascii_content(*byte))
    } else {
        body.iter().all(|byte| ascii_content(*byte))
    }
}

fn ascii_content(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\r' | 32..=126)
}
fn number(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(*byte - b'0'))
    })
}
fn date(value: &[u8]) -> bool {
    if value.len() != 20
        || value[19] != 0
        || [value[4], value[7], value[10], value[13], value[16]] != [b':', b':', b' ', b':', b':']
    {
        return false;
    }
    let Some(year) = number(&value[..4]) else {
        return false;
    };
    let Some(month) = number(&value[5..7]) else {
        return false;
    };
    let Some(day) = number(&value[8..10]) else {
        return false;
    };
    let Some(hour) = number(&value[11..13]) else {
        return false;
    };
    let Some(minute) = number(&value[14..16]) else {
        return false;
    };
    let Some(second) = number(&value[17..19]) else {
        return false;
    };
    let days = match month {
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return false,
    };
    year > 0 && (1..=days).contains(&day) && hour < 24 && minute < 60 && second < 60
}
fn offset_time(value: &[u8]) -> bool {
    value.len() == 7
        && matches!(value[0], b'+' | b'-')
        && value[3] == b':'
        && value[6] == 0
        && number(&value[1..3]).is_some_and(|hour| hour < 24)
        && number(&value[4..6]).is_some_and(|minute| minute < 60)
}

fn keep(raw: RawField<'_>, rule: Rule, endian: Endian) -> Result<Option<Field>, String> {
    let text = matches!(
        rule,
        Rule::Text | Rule::Copyright | Rule::Date | Rule::Offset | Rule::Subsecond
    );
    let valid = match rule {
        Rule::Text | Rule::Copyright => {
            raw.kind == 2 && ascii(raw.value, matches!(rule, Rule::Copyright))
        }
        Rule::Date => raw.kind == 2 && raw.count == 20 && date(raw.value),
        Rule::Offset => raw.kind == 2 && raw.count == 7 && offset_time(raw.value),
        Rule::Subsecond => {
            raw.kind == 2
                && (2..=17).contains(&raw.value.len())
                && raw.value.last() == Some(&0)
                && raw.value[..raw.value.len() - 1]
                    .iter()
                    .all(u8::is_ascii_digit)
        }
        Rule::Short => raw.kind == 3 && raw.count == 1,
        Rule::Rational => raw.kind == 5 && raw.count == 1 && endian.u32(&raw.value[4..]) != 0,
        Rule::SignedRational => {
            raw.kind == 10 && raw.count == 1 && endian.u32(&raw.value[4..]) != 0
        }
    };
    if !valid {
        return Ok(None);
    }
    let mut value = reserved(raw.value.len())?;
    if text {
        value.extend_from_slice(raw.value);
    } else if matches!(rule, Rule::Short) {
        value.extend_from_slice(&endian.u16(raw.value).to_le_bytes());
    } else {
        value.extend_from_slice(&endian.u32(raw.value).to_le_bytes());
        value.extend_from_slice(&endian.u32(&raw.value[4..]).to_le_bytes());
    }
    Ok(Some(Field {
        tag: raw.tag,
        kind: raw.kind,
        count: raw.count,
        value,
    }))
}

fn collect(
    directory: &Directory<'_>,
    photo: bool,
    endian: Endian,
    result: &mut SupportedExif,
    values: &mut usize,
    cancel: CancelCheck<'_>,
) -> Result<(), String> {
    for raw in &directory.fields {
        cancel()?;
        result.summary.excluded_gps |= raw.tag == 0x8825;
        result.summary.excluded_thumbnail |= matches!(raw.tag, 0x0201 | 0x0202);
        result.summary.excluded_private |= matches!(raw.tag, 0x927c | 0xc4a5 | 0xc634);
        let field = match rule(photo, raw.tag) {
            Some(rule) => keep(*raw, rule, endian)?,
            None => None,
        };
        if let Some(field) = field {
            *values = values
                .checked_add(field.value.len())
                .filter(|total| *total <= MAX_VALUES)
                .ok_or_else(|| {
                    message(
                        "EXIF保留字段超过32KiB",
                        "Retained EXIF fields exceed 32 KiB",
                    )
                })?;
            let target = if photo {
                &mut result.photo
            } else {
                &mut result.ifd0
            };
            target.try_reserve_exact(1).map_err(|_| allocation())?;
            target.push(field);
            result.summary.retained_fields += 1;
        } else {
            result.summary.dropped_fields += 1;
        }
    }
    Ok(())
}

pub fn inspect(tiff: &[u8], cancel: &AtomicBool) -> Result<SupportedExif, String> {
    inspect_checked(tiff, &|| check_cancel(cancel))
}

/// 与容器扫描共享取消检查；失败只表示此份EXIF不能用于保留。
pub(crate) fn inspect_checked(
    tiff: &[u8],
    cancel: CancelCheck<'_>,
) -> Result<SupportedExif, String> {
    cancel()?;
    if !(8..=MAX_INPUT).contains(&tiff.len()) {
        return Err(invalid());
    }
    let endian = match &tiff[..2] {
        b"II" => Endian(true),
        b"MM" => Endian(false),
        _ => return Err(invalid()),
    };
    if endian.u16(&tiff[2..]) != 42 {
        return Err(invalid());
    }
    let first = directory(tiff, endian.u32(&tiff[4..]), endian, cancel)?;
    let mut result = SupportedExif {
        ifd0: Vec::new(),
        photo: Vec::new(),
        summary: ExifSummary::default(),
    };
    let mut values = 0;
    if first.next != 0 {
        if first.next as usize == first.start {
            return Err(invalid());
        }
        range(tiff, first.next as usize, 6)?;
        if first.next < 8 {
            return Err(invalid());
        }
        result.summary.excluded_thumbnail = true;
    }
    collect(&first, false, endian, &mut result, &mut values, cancel)?;
    if let Some(pointer) = first.fields.iter().find(|field| field.tag == 0x8769) {
        if pointer.kind == 4 && pointer.count == 1 {
            let offset = endian.u32(pointer.value);
            let photo = directory(tiff, offset, endian, cancel)?;
            if photo.start < first.end && first.start < photo.end
                || photo.next != 0
                || photo.fields.iter().any(|field| field.tag == 0x8769)
            {
                return Err(invalid());
            }
            collect(&photo, true, endian, &mut result, &mut values, cancel)?;
        }
    }
    // 时区和亚秒只描述原拍摄日期；日期未通过验证时不留下孤立的附属字段。
    if !result.photo.iter().any(|field| field.tag == 0x9003) {
        let summary = &mut result.summary;
        result.photo.retain(|field| {
            if matches!(field.tag, 0x9011 | 0x9291) {
                summary.retained_fields -= 1;
                summary.dropped_fields += 1;
                false
            } else {
                true
            }
        });
    }
    cancel()?;
    Ok(result)
}

#[derive(Clone, Copy)]
struct OutputField<'a> {
    tag: u16,
    kind: u16,
    count: u32,
    value: &'a [u8],
}
impl<'a> From<&'a Field> for OutputField<'a> {
    fn from(field: &'a Field) -> Self {
        Self {
            tag: field.tag,
            kind: field.kind,
            count: field.count,
            value: &field.value,
        }
    }
}

impl SupportedExif {
    pub fn summary(&self) -> ExifSummary {
        self.summary
    }
    pub fn retained_bytes(&self) -> usize {
        (self.ifd0.capacity() + self.photo.capacity()) * std::mem::size_of::<Field>()
            + self
                .ifd0
                .iter()
                .chain(&self.photo)
                .map(|field| field.value.capacity())
                .sum::<usize>()
    }

    /// 方向与尺寸描述本次实际输出；不会再次复制源方向或源颜色空间声明。
    pub fn encode(
        &self,
        dimensions: (u32, u32),
        srgb: bool,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>, String> {
        check_cancel(cancel)?;
        if dimensions.0 == 0 || dimensions.1 == 0 {
            return Err(invalid());
        }
        let first_count = self.ifd0.len() + 2;
        let photo_count = self.photo.len() + 4;
        let photo_offset = 8 + 6 + first_count * 12;
        let data_offset = photo_offset + 6 + photo_count * 12;
        let orientation = 1u16.to_le_bytes();
        let pointer = (photo_offset as u32).to_le_bytes();
        let width = dimensions.0.to_le_bytes();
        let height = dimensions.1.to_le_bytes();
        let color = (if srgb { 1u16 } else { 65_535u16 }).to_le_bytes();
        let mut first: Vec<OutputField<'_>> = reserved(first_count)?;
        first.extend(self.ifd0.iter().map(OutputField::from));
        first.push(OutputField {
            tag: 0x0112,
            kind: 3,
            count: 1,
            value: &orientation,
        });
        first.push(OutputField {
            tag: 0x8769,
            kind: 4,
            count: 1,
            value: &pointer,
        });
        let mut photo: Vec<OutputField<'_>> = reserved(photo_count)?;
        photo.extend(self.photo.iter().map(OutputField::from));
        photo.extend([
            OutputField {
                tag: 0x9000,
                kind: 7,
                count: 4,
                value: b"0232",
            },
            OutputField {
                tag: 0xa001,
                kind: 3,
                count: 1,
                value: &color,
            },
            OutputField {
                tag: 0xa002,
                kind: 4,
                count: 1,
                value: &width,
            },
            OutputField {
                tag: 0xa003,
                kind: 4,
                count: 1,
                value: &height,
            },
        ]);
        first.sort_unstable_by_key(|field| field.tag);
        photo.sort_unstable_by_key(|field| field.tag);
        let mut output: Vec<u8> = reserved(data_offset)?;
        output.resize(data_offset, 0);
        output[..8].copy_from_slice(b"II\x2a\0\x08\0\0\0");
        for (start, fields) in [(8, first), (photo_offset, photo)] {
            check_cancel(cancel)?;
            output[start..start + 2].copy_from_slice(&(fields.len() as u16).to_le_bytes());
            for (index, field) in fields.into_iter().enumerate() {
                check_cancel(cancel)?;
                let row = start + 2 + index * 12;
                output[row..row + 2].copy_from_slice(&field.tag.to_le_bytes());
                output[row + 2..row + 4].copy_from_slice(&field.kind.to_le_bytes());
                output[row + 4..row + 8].copy_from_slice(&field.count.to_le_bytes());
                if field.value.len() <= 4 {
                    output[row + 8..row + 8 + field.value.len()].copy_from_slice(field.value);
                } else {
                    let length = output
                        .len()
                        .checked_add(field.value.len())
                        .and_then(|n| n.checked_add(field.value.len() & 1))
                        .filter(|n| *n <= MAX_OUTPUT)
                        .ok_or_else(invalid)?;
                    let value_offset = output.len() as u32;
                    output[row + 8..row + 12].copy_from_slice(&value_offset.to_le_bytes());
                    output
                        .try_reserve_exact(length - output.len())
                        .map_err(|_| allocation())?;
                    output.extend_from_slice(field.value);
                    if output.len() & 1 != 0 {
                        output.push(0);
                    }
                }
            }
        }
        check_cancel(cancel)?;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[derive(Clone)]
    struct Tag(u16, u16, u32, Vec<u8>);
    fn token() -> AtomicBool {
        AtomicBool::new(false)
    }
    fn text(tag: u16, bytes: &[u8]) -> Tag {
        Tag(tag, 2, bytes.len() as u32, bytes.to_vec())
    }
    fn put16(target: &mut [u8], n: u16, big: bool) {
        target[..2].copy_from_slice(&if big {
            n.to_be_bytes()
        } else {
            n.to_le_bytes()
        });
    }
    fn put32(target: &mut [u8], n: u32, big: bool) {
        target[..4].copy_from_slice(&if big {
            n.to_be_bytes()
        } else {
            n.to_le_bytes()
        });
    }
    fn numbers(tag: u16, kind: u16, values: &[u32], big: bool) -> Tag {
        let mut bytes = Vec::new();
        for &n in values {
            if kind == 3 {
                bytes.extend_from_slice(&if big {
                    (n as u16).to_be_bytes()
                } else {
                    (n as u16).to_le_bytes()
                });
            } else {
                bytes.extend_from_slice(&if big {
                    n.to_be_bytes()
                } else {
                    n.to_le_bytes()
                });
            }
        }
        let count = if matches!(kind, 5 | 10) {
            values.len() / 2
        } else {
            values.len()
        };
        Tag(tag, kind, count as u32, bytes)
    }
    fn fixture(big: bool, mut first: Vec<Tag>, photo: Vec<Tag>) -> Vec<u8> {
        let photo_start = 8 + 6 + (first.len() + usize::from(!photo.is_empty())) * 12;
        if !photo.is_empty() {
            first.push(numbers(0x8769, 4, &[photo_start as u32], big));
        }
        let mut bytes = vec![
            0;
            if photo.is_empty() {
                photo_start
            } else {
                photo_start + 6 + photo.len() * 12
            }
        ];
        bytes[..2].copy_from_slice(if big { b"MM" } else { b"II" });
        put16(&mut bytes[2..], 42, big);
        put32(&mut bytes[4..], 8, big);
        for (start, tags) in [(8, first), (photo_start, photo)] {
            if tags.is_empty() {
                continue;
            }
            put16(&mut bytes[start..], tags.len() as u16, big);
            for (index, Tag(tag, kind, count, value)) in tags.into_iter().enumerate() {
                let row = start + 2 + index * 12;
                put16(&mut bytes[row..], tag, big);
                put16(&mut bytes[row + 2..], kind, big);
                put32(&mut bytes[row + 4..], count, big);
                if value.len() <= 4 {
                    bytes[row + 8..row + 8 + value.len()].copy_from_slice(&value);
                } else {
                    let position = bytes.len() as u32;
                    put32(&mut bytes[row + 8..], position, big);
                    bytes.extend_from_slice(&value);
                    if bytes.len() % 2 != 0 {
                        bytes.push(0);
                    }
                }
            }
        }
        bytes
    }
    // 独立按目录定位生成结果，断言源字段值以及重新生成的数值和指针。
    fn output_tags(bytes: &[u8], start: usize) -> (Vec<Tag>, u32) {
        let n = u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap()) as usize;
        let mut tags = Vec::new();
        for index in 0..n {
            let row = start + 2 + index * 12;
            let tag = u16::from_le_bytes(bytes[row..row + 2].try_into().unwrap());
            let kind = u16::from_le_bytes(bytes[row + 2..row + 4].try_into().unwrap());
            let count = u32::from_le_bytes(bytes[row + 4..row + 8].try_into().unwrap());
            let length = count as usize
                * match kind {
                    3 => 2,
                    4 => 4,
                    5 | 10 => 8,
                    _ => 1,
                };
            let position = if length <= 4 {
                row + 8
            } else {
                u32::from_le_bytes(bytes[row + 8..row + 12].try_into().unwrap()) as usize
            };
            assert!(length <= 4 || position % 2 == 0);
            tags.push(Tag(
                tag,
                kind,
                count,
                bytes[position..position + length].to_vec(),
            ));
        }
        let next = start + 2 + n * 12;
        (
            tags,
            u32::from_le_bytes(bytes[next..next + 4].try_into().unwrap()),
        )
    }
    fn find(tags: &[Tag], tag: u16) -> &[u8] {
        &tags.iter().find(|field| field.0 == tag).unwrap().3
    }
    fn output(bytes: &[u8]) -> (Vec<Tag>, Vec<Tag>) {
        assert_eq!(&bytes[..8], b"II\x2a\0\x08\0\0\0");
        let (first, next) = output_tags(bytes, 8);
        assert_eq!(next, 0);
        let photo = u32::from_le_bytes(find(&first, 0x8769).try_into().unwrap()) as usize;
        let (photo, next) = output_tags(bytes, photo);
        assert_eq!(next, 0);
        assert!(first.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(photo.windows(2).all(|pair| pair[0].0 < pair[1].0));
        (first, photo)
    }

    #[test]
    fn image_exif_both_endians_rebuild_values_dimensions_and_orientation() {
        for big in [false, true] {
            let bytes = fixture(
                big,
                vec![
                    text(0x010f, b"Example Camera\0"),
                    numbers(0x0112, 3, &[6], big),
                    numbers(0x0100, 4, &[99], big),
                    numbers(0x0101, 4, &[77], big),
                ],
                vec![
                    text(0x9003, b"2024:02:29 23:45:56\0"),
                    text(0x9011, b"-07:30\0"),
                    text(0x9291, b"007\0"),
                    numbers(0x829d, 5, &[7, 2], big),
                    numbers(0x9204, 10, &[1, (-3i32) as u32], big),
                    numbers(0x8827, 3, &[640], big),
                ],
            );
            let kept = inspect(&bytes, &token()).unwrap();
            assert_eq!(kept.summary().retained_fields, 7);
            assert_eq!(kept.summary().dropped_fields, 4);
            for srgb in [false, true] {
                let encoded = kept.encode((3, 2), srgb, &token()).unwrap();
                let (first, photo) = output(&encoded);
                assert_eq!(find(&first, 0x010f), b"Example Camera\0");
                assert_eq!(find(&first, 0x0112), 1u16.to_le_bytes());
                assert!(!first.iter().any(|tag| matches!(tag.0, 0x0100 | 0x0101)));
                assert_eq!(find(&photo, 0xa002), 3u32.to_le_bytes());
                assert_eq!(find(&photo, 0xa003), 2u32.to_le_bytes());
                assert_eq!(
                    find(&photo, 0xa001),
                    (if srgb { 1u16 } else { 65535 }).to_le_bytes()
                );
                assert_eq!(find(&photo, 0x9000), b"0232");
                assert_eq!(find(&photo, 0x829d), [7, 0, 0, 0, 2, 0, 0, 0]);
                assert_eq!(find(&photo, 0x9204), [1, 0, 0, 0, 253, 255, 255, 255]);
                assert_eq!(find(&photo, 0x8827), 640u16.to_le_bytes());
                let again = inspect(&encoded, &token()).unwrap();
                assert_eq!(again.summary().retained_fields, 7);
                assert_eq!(again.encode((3, 2), srgb, &token()).unwrap(), encoded);
            }
        }
    }

    #[test]
    fn image_exif_excluded_directories_private_and_unknown_do_not_return() {
        let mut bytes = fixture(
            false,
            vec![text(0x010f, b"Keep\0"), numbers(0x8825, 4, &[0], false)],
            vec![
                Tag(0x927c, 7, 6, b"SECRET".to_vec()),
                Tag(0x9286, 7, 7, b"COMMENT".to_vec()),
                text(0xffff, b"unknown\0"),
            ],
        );
        let thumbnail = bytes.len() as u32;
        bytes.extend_from_slice(&[0; 6]);
        let next = 8 + 2 + 3 * 12;
        put32(&mut bytes[next..], thumbnail, false);
        let kept = inspect(&bytes, &token()).unwrap();
        assert_eq!(
            kept.summary(),
            ExifSummary {
                retained_fields: 1,
                dropped_fields: 5,
                excluded_gps: true,
                excluded_thumbnail: true,
                excluded_private: true
            }
        );
        let (first, photo) = output(&kept.encode((1, 1), true, &token()).unwrap());
        assert_eq!(
            first.iter().map(|tag| tag.0).collect::<Vec<_>>(),
            [0x010f, 0x0112, 0x8769]
        );
        assert_eq!(
            photo.iter().map(|tag| tag.0).collect::<Vec<_>>(),
            [0x9000, 0xa001, 0xa002, 0xa003]
        );
    }

    #[test]
    fn image_exif_bad_values_drop_individually_and_orphan_dates_do_not_survive() {
        let bytes = fixture(
            false,
            vec![
                text(0x010f, b"valid\0"),
                text(0x0110, b"bad\xff\0"),
                text(0x013b, b"no null"),
            ],
            vec![
                text(0x9003, b"2023:02:29 12:34:56\0"),
                text(0x9011, b"+01:00\0"),
                text(0x9291, b"123\0"),
                numbers(0x829a, 5, &[1, 0], false),
                numbers(0x9204, 10, &[1, 0], false),
                numbers(0x8827, 3, &[100, 200], false),
            ],
        );
        let kept = inspect(&bytes, &token()).unwrap();
        assert_eq!(kept.summary().retained_fields, 1);
        assert_eq!(kept.summary().dropped_fields, 9);
        let (_, photo) = output(&kept.encode((1, 1), true, &token()).unwrap());
        assert!(!photo
            .iter()
            .any(|tag| matches!(tag.0, 0x9003 | 0x9011 | 0x9291 | 0x829a | 0x9204 | 0x8827)));
        let copyright = fixture(
            false,
            vec![
                text(0x8298, b"Photographer\0Editor\0"),
                text(0x010e, b"First line\r\nSecond\tcolumn\nLast line\0"),
            ],
            Vec::new(),
        );
        let kept = inspect(&copyright, &token()).unwrap();
        assert_eq!(kept.summary().retained_fields, 2);
        assert_eq!(
            find(
                &output(&kept.encode((1, 1), true, &token()).unwrap()).0,
                0x010e
            ),
            b"First line\r\nSecond\tcolumn\nLast line\0"
        );
        assert_eq!(
            find(
                &output(&kept.encode((1, 1), true, &token()).unwrap()).0,
                0x8298
            ),
            b"Photographer\0Editor\0"
        );
    }

    #[test]
    fn image_exif_bad_structures_cycles_duplicates_and_ranges_are_rejected() {
        let valid = fixture(
            false,
            vec![text(0x010f, b"Camera\0"), text(0x0110, b"Model\0")],
            vec![text(0x9003, b"2024:01:02 03:04:05\0")],
        );
        let mut cases = vec![valid[..7].to_vec(), vec![0; MAX_INPUT + 1]];
        let mut bytes = valid.clone();
        put16(&mut bytes[2..], 43, false);
        cases.push(bytes);
        let mut bytes = valid.clone();
        put32(&mut bytes[4..], u32::MAX, false);
        cases.push(bytes);
        let mut bytes = valid.clone();
        put16(&mut bytes[8..], 257, false);
        cases.push(bytes);
        let mut bytes = valid.clone();
        put16(&mut bytes[22..], 0x010f, false);
        cases.push(bytes);
        let mut bytes = valid.clone();
        put32(&mut bytes[18..], u32::MAX, false);
        cases.push(bytes);
        let mut bytes = valid.clone();
        put16(&mut bytes[12..], u16::MAX, false);
        cases.push(bytes);
        let pointer_row = 8 + 2 + 2 * 12;
        let mut bytes = valid.clone();
        put32(&mut bytes[pointer_row + 8..], 8, false);
        cases.push(bytes);
        let photo = u32::from_le_bytes(valid[pointer_row + 8..pointer_row + 12].try_into().unwrap())
            as usize;
        let mut bytes = valid.clone();
        put32(&mut bytes[photo + 14..], 8, false);
        cases.push(bytes);
        for bytes in cases {
            assert!(inspect(&bytes, &token()).is_err());
        }
    }

    #[test]
    fn image_exif_string_and_input_budgets_and_empty_supported_set() {
        let mut text_bytes = vec![b'x'; MAX_STRING - 1];
        text_bytes.push(0);
        let first = [0x010e, 0x010f, 0x0110, 0x013b, 0x8298]
            .into_iter()
            .map(|tag| text(tag, &text_bytes))
            .collect();
        let photo = [0xa433, 0xa434]
            .into_iter()
            .map(|tag| text(tag, &text_bytes))
            .collect();
        let bytes = fixture(false, first, photo);
        let kept = inspect(&bytes, &token()).unwrap();
        assert_eq!(kept.summary().retained_fields, 7);
        assert!(kept.retained_bytes() >= 7 * MAX_STRING);
        assert!(kept.retained_bytes() <= MAX_VALUES);
        assert!(kept.encode((1, 1), true, &token()).unwrap().len() < MAX_OUTPUT);
        let mut too_long = text_bytes;
        too_long.insert(0, b'y');
        let mut bytes = fixture(false, vec![text(0x010f, &too_long)], Vec::new());
        bytes.resize(MAX_INPUT, 0);
        let empty = inspect(&bytes, &token()).unwrap();
        assert_eq!(empty.summary().retained_fields, 0);
        assert_eq!(empty.summary().dropped_fields, 1);
        assert_eq!(empty.retained_bytes(), 0);
        output(&empty.encode((1, 1), true, &token()).unwrap());
        bytes.push(0);
        assert!(inspect(&bytes, &token()).is_err());
        for dims in [(0, 1), (1, 0)] {
            assert!(kept.encode(dims, true, &token()).is_err());
        }
        let large = empty
            .encode((u32::MAX, 2_000_000_000), false, &token())
            .unwrap();
        let (_, photo) = output(&large);
        assert_eq!(find(&photo, 0xa002), u32::MAX.to_le_bytes());
        assert_eq!(find(&photo, 0xa003), 2_000_000_000u32.to_le_bytes());
    }

    #[test]
    fn image_exif_cancelled_inspection_and_encode_never_publish_partial_results() {
        let bytes = fixture(
            false,
            vec![text(0x010f, b"Camera\0"), text(0x0110, b"Model\0")],
            vec![text(0x9003, b"2024:01:02 03:04:05\0")],
        );
        assert!(inspect(&bytes, &AtomicBool::new(true)).is_err());
        let checkpoints = Cell::new(0);
        let error = inspect_checked(&bytes, &|| {
            checkpoints.set(checkpoints.get() + 1);
            if checkpoints.get() == 5 {
                Err("cancel checkpoint".to_string())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error, "cancel checkpoint");
        let kept = inspect(&bytes, &token()).unwrap();
        assert!(kept.encode((3, 2), true, &AtomicBool::new(true)).is_err());
        assert!(kept.encode((3, 2), true, &token()).is_ok());
    }
}
