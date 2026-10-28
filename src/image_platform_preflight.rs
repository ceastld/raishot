//! 系统解码前的受控静态BMFF目录检查；只对所需EXIF方向做有界重组，不读取像素。
//! 目录声明与实际码流头分别校验；系统内部解码峰值仍由codec实现决定。
use super::{
    coded::{self, ByteView, Cicp, CodedColor, CodedInfo, SampleModel},
    PlatformError, PlatformFormat, MAX_BYTES, MAX_PIXELS, MAX_SIDE,
};
use std::{ops::Range, sync::atomic::AtomicBool};

#[path = "image_platform_item.rs"]
mod item;

const MAX_BOXES: usize = 4096;
const MAX_ITEMS: usize = 128;
const MAX_PROPERTIES: usize = 1024;
const MAX_ASSOCIATIONS: usize = 64;
const MAX_EXTENTS: usize = MAX_ITEMS * 64;
const MAX_METADATA: u64 = 8 * 1024 * 1024;

fn invalid() -> PlatformError {
    PlatformError::Invalid {
        hresult: None,
        message: tr!(
            "系统图片BMFF目录、关联或范围无效",
            "Invalid system image BMFF directory, association, or range"
        )
        .into(),
    }
}

fn unsupported() -> PlatformError {
    PlatformError::Unsupported {
        hresult: None,
        message: tr!(
            "系统图片预检仅支持有完整尺寸关联的独立静态图项；此结构暂不支持",
            "System image preflight requires independent still items with complete dimension associations; this structure is unsupported"
        ).into(),
    }
}

fn budget() -> PlatformError {
    PlatformError::Budget(
        tr!(
            "系统图片声明尺寸或目录超过预检预算",
            "System image dimensions or directory exceed the preflight budget"
        )
        .into(),
    )
}

fn push<T>(values: &mut Vec<T>, value: T, limit: usize) -> Result<(), PlatformError> {
    if values.len() >= limit {
        return Err(budget());
    }
    if values.len() == values.capacity() {
        let capacity = values.capacity().max(8).saturating_mul(2).min(limit);
        values
            .try_reserve_exact(capacity - values.len())
            .map_err(|_| budget())?;
    }
    values.push(value);
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }
    fn take(&mut self, length: usize) -> Result<&'a [u8], PlatformError> {
        let end = self
            .position
            .checked_add(length)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(invalid)?;
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }
    fn uint(&mut self, length: usize) -> Result<u64, PlatformError> {
        if length > 8 {
            return Err(unsupported());
        }
        Ok(self
            .take(length)?
            .iter()
            .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)))
    }
    fn full(&mut self) -> Result<(u8, u32), PlatformError> {
        let value = self.uint(4)? as u32;
        Ok(((value >> 24) as u8, value & 0x00ff_ffff))
    }
    fn string(&mut self) -> Result<&'a [u8], PlatformError> {
        let rest = &self.bytes[self.position..];
        let length = rest
            .iter()
            .take(4097)
            .position(|byte| *byte == 0)
            .ok_or_else(unsupported)?;
        let value = self.take(length + 1)?;
        Ok(&value[..length])
    }
    fn finish(&self) -> Result<(), PlatformError> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid())
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct BoxInfo {
    kind: [u8; 4],
    start: usize,
    body: usize,
    end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemKind {
    Av1,
    Hevc,
    Exif,
    Xmp,
}
impl ItemKind {
    fn image(self) -> bool {
        matches!(self, Self::Av1 | Self::Hevc)
    }
}
struct Item {
    id: u32,
    kind: ItemKind,
    hidden: bool,
}
#[derive(Clone, Copy)]
enum Property {
    Dimensions((u32, u32), BoxInfo),
    Av1(BoxInfo),
    Hevc(BoxInfo),
    Pixel(u8, [u8; 4], BoxInfo),
    Color(ColorProperty, BoxInfo),
    Other(BoxInfo),
}

#[derive(Clone, Copy)]
enum ColorProperty {
    Nclx(Cicp),
    Icc,
}
impl Property {
    fn info(self) -> BoxInfo {
        match self {
            Self::Dimensions(_, info)
            | Self::Av1(info)
            | Self::Hevc(info)
            | Self::Pixel(_, _, info)
            | Self::Color(_, info)
            | Self::Other(info) => info,
        }
    }
}
#[derive(Clone, Copy)]
struct Association {
    id: u32,
    indexes: [u16; MAX_ASSOCIATIONS],
    essential: [bool; MAX_ASSOCIATIONS],
    count: usize,
}
struct Location {
    id: u32,
    length: u64,
    extents: Range<usize>,
}
struct Reference {
    from: u32,
    to: u32,
}

struct Scan<'a> {
    bytes: &'a [u8],
    cancel: &'a AtomicBool,
    max_output_bytes: u64,
    boxes: usize,
    metadata: u64,
    primary: Option<u32>,
    items: Vec<Item>,
    properties: Vec<Property>,
    associations: Vec<Association>,
    locations: Vec<Location>,
    extents: Vec<Range<usize>>,
    media: Vec<Range<usize>>,
    references: Vec<Reference>,
}

impl Scan<'_> {
    fn next(&mut self, position: &mut usize, end: usize) -> Result<BoxInfo, PlatformError> {
        super::cancelled(self.cancel)?;
        self.boxes = self
            .boxes
            .checked_add(1)
            .filter(|count| *count <= MAX_BOXES)
            .ok_or_else(budget)?;
        let start = *position;
        if start > end || end - start < 8 {
            return Err(invalid());
        }
        let mut cursor = Cursor::new(&self.bytes[start..end]);
        let short = cursor.uint(4)?;
        let kind: [u8; 4] = cursor.take(4)?.try_into().unwrap();
        let length = match short {
            1 => cursor.uint(8)?,
            0 if kind == *b"mdat" && end == self.bytes.len() => (end - start) as u64,
            0 => return Err(unsupported()),
            _ => short,
        };
        if length < cursor.position as u64 {
            return Err(invalid());
        }
        let box_end = (start as u64)
            .checked_add(length)
            .filter(|value| *value <= end as u64)
            .ok_or_else(invalid)? as usize;
        let info = BoxInfo {
            kind,
            start,
            body: start + cursor.position,
            end: box_end,
        };
        *position = box_end;
        Ok(info)
    }
    fn payload(&self, info: BoxInfo) -> &[u8] {
        &self.bytes[info.body..info.end]
    }
    fn charge(&mut self, length: u64) -> Result<(), PlatformError> {
        self.metadata = self
            .metadata
            .checked_add(length)
            .filter(|value| *value <= MAX_METADATA)
            .ok_or_else(budget)?;
        Ok(())
    }
    fn full_children(&self, info: BoxInfo) -> Result<usize, PlatformError> {
        let mut cursor = Cursor::new(self.payload(info));
        if cursor.full()? != (0, 0) {
            return Err(unsupported());
        }
        Ok(info.body + cursor.position)
    }
    fn dimensions(&self, bytes: &[u8]) -> Result<(u32, u32), PlatformError> {
        let mut cursor = Cursor::new(bytes);
        if cursor.full()? != (0, 0) {
            return Err(unsupported());
        }
        let width = cursor.uint(4)?;
        let height = cursor.uint(4)?;
        cursor.finish()?;
        let pixels = width.checked_mul(height).ok_or_else(budget)?;
        if width == 0
            || height == 0
            || width > u64::from(MAX_SIDE)
            || height > u64::from(MAX_SIDE)
            || pixels > MAX_PIXELS
            || pixels
                .checked_mul(4)
                .is_none_or(|bytes| bytes > self.max_output_bytes)
        {
            return Err(budget());
        }
        Ok((width as u32, height as u32))
    }
    fn property(&self, info: BoxInfo) -> Result<Property, PlatformError> {
        let bytes = self.payload(info);
        match &info.kind {
            b"ispe" => Ok(Property::Dimensions(self.dimensions(bytes)?, info)),
            b"av1C" => {
                // 暂不接收配置中的OBU、分层选择或未证明的附加结构。
                if bytes.len() != 4 {
                    return Err(unsupported());
                }
                if bytes[0] != 0x81
                    || bytes[1] >> 5 > 2
                    || bytes[3] & 0xe0 != 0
                    || bytes[3] & 0x10 == 0 && bytes[3] & 0x0f != 0
                {
                    return Err(invalid());
                }
                Ok(Property::Av1(info))
            }
            b"hvcC" => {
                Self::hevc_configuration(bytes)?;
                Ok(Property::Hevc(info))
            }
            b"pixi" => {
                let mut cursor = Cursor::new(bytes);
                if cursor.full()? != (0, 0) {
                    return Err(unsupported());
                }
                let channels = cursor.uint(1)? as usize;
                if !(1..=4).contains(&channels) {
                    return Err(unsupported());
                }
                let values = cursor.take(channels)?;
                if values.iter().any(|bits| !(1..=16).contains(bits)) {
                    return Err(unsupported());
                }
                cursor.finish()?;
                let mut bits = [0; 4];
                bits[..channels].copy_from_slice(values);
                Ok(Property::Pixel(channels as u8, bits, info))
            }
            b"irot" if bytes.len() == 1 && bytes[0] == 0 => Ok(Property::Other(info)),
            b"pasp" => {
                let mut cursor = Cursor::new(bytes);
                let horizontal = cursor.uint(4)?;
                let vertical = cursor.uint(4)?;
                cursor.finish()?;
                if horizontal == 0 || horizontal != vertical {
                    return Err(unsupported());
                }
                Ok(Property::Other(info))
            }
            b"colr" => {
                if bytes.len() < 4 {
                    return Err(invalid());
                }
                match &bytes[..4] {
                    b"nclx" if bytes.len() == 11 && bytes[10] & 0x7f == 0 => Ok(Property::Color(
                        ColorProperty::Nclx(Cicp {
                            primaries: u16::from_be_bytes([bytes[4], bytes[5]]),
                            transfer: u16::from_be_bytes([bytes[6], bytes[7]]),
                            matrix: u16::from_be_bytes([bytes[8], bytes[9]]),
                            full_range: bytes[10] & 0x80 != 0,
                        }),
                        info,
                    )),
                    b"prof" | b"rICC" if (5..=4 * 1024 * 1024 + 4).contains(&bytes.len()) => {
                        Ok(Property::Color(ColorProperty::Icc, info))
                    }
                    _ => return Err(unsupported()),
                }
            }
            _ => Err(unsupported()),
        }
    }
    fn hevc_configuration(bytes: &[u8]) -> Result<(), PlatformError> {
        if bytes.len() < 23 {
            return Err(invalid());
        }
        if bytes.len() > 64 * 1024 {
            return Err(budget());
        }
        if bytes[0] != 1
            || bytes[13] & 0xf0 != 0xf0
            || bytes[15] & 0xfc != 0xfc
            || bytes[16] & 0xfc != 0xfc
            || bytes[17] & 0xf8 != 0xf8
            || bytes[18] & 0xf8 != 0xf8
        {
            return Err(invalid());
        }
        let mut cursor = Cursor::new(&bytes[22..]);
        let arrays = cursor.uint(1)?;
        if arrays > 32 {
            return Err(budget());
        }
        let mut total = 0u64;
        for _ in 0..arrays {
            let kind = cursor.uint(1)? as u8;
            if kind & 0x40 != 0 {
                return Err(invalid());
            }
            if !matches!(kind & 0x3f, 32 | 33 | 34 | 39 | 40) {
                return Err(unsupported());
            }
            let count = cursor.uint(2)?;
            total = total
                .checked_add(count)
                .filter(|total| *total <= 256)
                .ok_or_else(budget)?;
            for _ in 0..count {
                let length = cursor.uint(2)? as usize;
                if length < 2 {
                    return Err(invalid());
                }
                let nalu = cursor.take(length)?;
                if (nalu[0] >> 1) & 0x3f != kind & 0x3f {
                    return Err(invalid());
                }
            }
        }
        cursor.finish()
    }
    fn ipco(&mut self, info: BoxInfo) -> Result<(), PlatformError> {
        let mut position = info.body;
        while position < info.end {
            let property = self.next(&mut position, info.end)?;
            let parsed = self.property(property)?;
            push(&mut self.properties, parsed, MAX_PROPERTIES)?;
        }
        Ok(())
    }
    fn ipma(&mut self, info: BoxInfo) -> Result<(), PlatformError> {
        let mut cursor = Cursor::new(self.payload(info));
        let (version, flags) = cursor.full()?;
        if version > 1 || flags & !1 != 0 {
            return Err(unsupported());
        }
        let count = cursor.uint(4)?;
        if count == 0 || count > MAX_ITEMS as u64 {
            return Err(budget());
        }
        let mut parsed = Vec::new();
        for _ in 0..count {
            super::cancelled(self.cancel)?;
            let id = cursor.uint(if version == 0 { 2 } else { 4 })? as u32;
            if id == 0
                || parsed
                    .iter()
                    .any(|association: &Association| association.id == id)
            {
                return Err(invalid());
            }
            let count = cursor.uint(1)? as usize;
            if count > MAX_ASSOCIATIONS {
                return Err(budget());
            }
            let mut association = Association {
                id,
                indexes: [0; MAX_ASSOCIATIONS],
                essential: [false; MAX_ASSOCIATIONS],
                count,
            };
            for position in 0..count {
                let (value, mask) = if flags & 1 == 0 {
                    (cursor.uint(1)? as u16, 0x80)
                } else {
                    (cursor.uint(2)? as u16, 0x8000)
                };
                association.indexes[position] = value & (mask - 1);
                association.essential[position] = value & mask != 0;
            }
            push(&mut parsed, association, MAX_ITEMS)?;
        }
        cursor.finish()?;
        self.associations = parsed;
        Ok(())
    }
    fn iprp(&mut self, info: BoxInfo) -> Result<(), PlatformError> {
        let mut position = info.body;
        let (mut container, mut association) = (false, false);
        while position < info.end {
            let child = self.next(&mut position, info.end)?;
            match &child.kind {
                b"ipco" if !container => {
                    self.ipco(child)?;
                    container = true;
                }
                b"ipma" if !association => {
                    self.ipma(child)?;
                    association = true;
                }
                b"ipco" | b"ipma" => return Err(invalid()),
                _ => return Err(unsupported()),
            }
        }
        if !container || !association {
            return Err(unsupported());
        }
        Ok(())
    }
    fn infe(&self, info: BoxInfo) -> Result<Item, PlatformError> {
        let mut cursor = Cursor::new(self.payload(info));
        let (version, flags) = cursor.full()?;
        if !matches!(version, 2 | 3) || flags & !1 != 0 {
            return Err(unsupported());
        }
        let id = cursor.uint(if version == 2 { 2 } else { 4 })? as u32;
        if id == 0 {
            return Err(invalid());
        }
        if cursor.uint(2)? != 0 {
            return Err(unsupported());
        }
        let kind = match cursor.take(4)? {
            b"av01" => ItemKind::Av1,
            b"hvc1" => ItemKind::Hevc,
            b"Exif" => ItemKind::Exif,
            b"mime" => ItemKind::Xmp,
            _ => return Err(unsupported()),
        };
        cursor.string()?;
        if kind == ItemKind::Xmp {
            if cursor.string()? != b"application/rdf+xml" {
                return Err(unsupported());
            }
            if cursor.position < cursor.bytes.len() && !cursor.string()?.is_empty() {
                return Err(unsupported());
            }
        }
        cursor.finish()?;
        Ok(Item {
            id,
            kind,
            hidden: flags & 1 != 0,
        })
    }
    fn iinf(&mut self, info: BoxInfo) -> Result<(), PlatformError> {
        let mut cursor = Cursor::new(self.payload(info));
        let (version, flags) = cursor.full()?;
        if version > 1 || flags != 0 {
            return Err(unsupported());
        }
        let count = cursor.uint(if version == 0 { 2 } else { 4 })?;
        if count == 0 || count > MAX_ITEMS as u64 {
            return Err(budget());
        }
        let mut position = info.body + cursor.position;
        for _ in 0..count {
            let child = self.next(&mut position, info.end)?;
            if child.kind != *b"infe" {
                return Err(unsupported());
            }
            let item = self.infe(child)?;
            if self.items.iter().any(|other| other.id == item.id) {
                return Err(invalid());
            }
            push(&mut self.items, item, MAX_ITEMS)?;
        }
        if position != info.end {
            return Err(invalid());
        }
        Ok(())
    }
    fn iloc(&mut self, info: BoxInfo) -> Result<(), PlatformError> {
        let mut cursor = Cursor::new(self.payload(info));
        let (version, flags) = cursor.full()?;
        if version > 2 || flags != 0 {
            return Err(unsupported());
        }
        let sizes = cursor.take(2)?;
        let offset_size = usize::from(sizes[0] >> 4);
        let length_size = usize::from(sizes[0] & 0xf);
        let base_size = usize::from(sizes[1] >> 4);
        if sizes[1] & 0xf != 0
            || !matches!(offset_size, 0 | 4 | 8)
            || !matches!(length_size, 4 | 8)
            || !matches!(base_size, 0 | 4 | 8)
        {
            return Err(unsupported());
        }
        let count = cursor.uint(if version < 2 { 2 } else { 4 })?;
        if count == 0 || count > MAX_ITEMS as u64 {
            return Err(budget());
        }
        let mut locations = Vec::new();
        let mut extents = Vec::new();
        for _ in 0..count {
            super::cancelled(self.cancel)?;
            let id = cursor.uint(if version < 2 { 2 } else { 4 })? as u32;
            if id == 0
                || locations
                    .iter()
                    .any(|location: &Location| location.id == id)
            {
                return Err(invalid());
            }
            if version > 0 && cursor.uint(2)? != 0 {
                return Err(unsupported());
            }
            if cursor.uint(2)? != 0 {
                return Err(unsupported());
            }
            let base = cursor.uint(base_size)?;
            let extent_count = cursor.uint(2)?;
            if extent_count == 0 || extent_count > 64 {
                return Err(budget());
            }
            let mut length = 0u64;
            let first_extent = extents.len();
            for _ in 0..extent_count {
                super::cancelled(self.cancel)?;
                let offset = base
                    .checked_add(cursor.uint(offset_size)?)
                    .ok_or_else(invalid)?;
                let size = cursor.uint(length_size)?;
                let end = offset
                    .checked_add(size)
                    .filter(|end| *end <= self.bytes.len() as u64)
                    .ok_or_else(invalid)?;
                if size == 0 {
                    return Err(unsupported());
                }
                length = length.checked_add(size).ok_or_else(invalid)?;
                push(&mut extents, offset as usize..end as usize, MAX_EXTENTS)?;
            }
            push(
                &mut locations,
                Location {
                    id,
                    length,
                    extents: first_extent..extents.len(),
                },
                MAX_ITEMS,
            )?;
        }
        cursor.finish()?;
        self.locations = locations;
        self.extents = extents;
        Ok(())
    }
    fn iref(&mut self, info: BoxInfo) -> Result<(), PlatformError> {
        let mut cursor = Cursor::new(self.payload(info));
        let (version, flags) = cursor.full()?;
        if version > 1 || flags != 0 {
            return Err(unsupported());
        }
        let mut position = info.body + cursor.position;
        while position < info.end {
            let child = self.next(&mut position, info.end)?;
            if child.kind != *b"cdsc" {
                return Err(unsupported());
            }
            let mut cursor = Cursor::new(self.payload(child));
            let from = cursor.uint(if version == 0 { 2 } else { 4 })? as u32;
            let count = cursor.uint(2)?;
            if count == 0 || count > MAX_ITEMS as u64 {
                return Err(budget());
            }
            let mut parsed = Vec::new();
            for _ in 0..count {
                let to = cursor.uint(if version == 0 { 2 } else { 4 })? as u32;
                if self
                    .references
                    .iter()
                    .chain(parsed.iter())
                    .any(|entry: &Reference| entry.from == from && entry.to == to)
                {
                    return Err(invalid());
                }
                push(&mut parsed, Reference { from, to }, MAX_ITEMS)?;
            }
            cursor.finish()?;
            for reference in parsed {
                push(&mut self.references, reference, 1024)?;
            }
        }
        Ok(())
    }
    fn meta(&mut self, info: BoxInfo) -> Result<(), PlatformError> {
        let mut position = self.full_children(info)?;
        let mut seen = Vec::new();
        while position < info.end {
            let child = self.next(&mut position, info.end)?;
            if seen.contains(&child.kind) {
                return Err(invalid());
            }
            push(&mut seen, child.kind, 8)?;
            match &child.kind {
                b"hdlr" => {
                    let mut cursor = Cursor::new(self.payload(child));
                    if cursor.full()? != (0, 0)
                        || cursor.uint(4)? != 0
                        || cursor.take(4)? != b"pict"
                        || cursor.take(12)? != [0; 12]
                    {
                        return Err(unsupported());
                    }
                    if cursor.position < cursor.bytes.len() {
                        cursor.string()?;
                    }
                    cursor.finish()?;
                }
                b"pitm" => {
                    let mut cursor = Cursor::new(self.payload(child));
                    let (version, flags) = cursor.full()?;
                    if version > 1 || flags != 0 {
                        return Err(unsupported());
                    }
                    let id = cursor.uint(if version == 0 { 2 } else { 4 })? as u32;
                    cursor.finish()?;
                    self.primary = Some(id);
                }
                b"iinf" => self.iinf(child)?,
                b"iloc" => self.iloc(child)?,
                b"iprp" => self.iprp(child)?,
                b"iref" => self.iref(child)?,
                _ => return Err(unsupported()),
            }
        }
        if [*b"hdlr", *b"pitm", *b"iinf", *b"iloc", *b"iprp"]
            .iter()
            .any(|kind| !seen.contains(kind))
        {
            return Err(unsupported());
        }
        Ok(())
    }
    fn item_orientation(
        &self,
        id: u32,
    ) -> Result<Option<image::metadata::Orientation>, PlatformError> {
        let mut selected = None;
        for reference in self
            .references
            .iter()
            .filter(|reference| reference.to == id)
        {
            super::cancelled(self.cancel)?;
            let item = self
                .items
                .iter()
                .find(|item| item.id == reference.from)
                .ok_or_else(invalid)?;
            if item.kind != ItemKind::Exif {
                continue;
            }
            if selected.replace(item.id).is_some() {
                return Err(coded::unsupported(
                    "图项关联了多份EXIF，无法确定方向",
                    "Multiple EXIF items leave image orientation ambiguous",
                ));
            }
        }
        let Some(id) = selected else { return Ok(None) };
        let location = self
            .locations
            .iter()
            .find(|location| location.id == id)
            .ok_or_else(invalid)?;
        let length = usize::try_from(location.length).map_err(|_| budget())?;
        if length > 256 * 1024 + 4 {
            return Err(budget());
        }
        let mut payload = Vec::new();
        let allocation = payload.try_reserve_exact(length);
        super::cancelled(self.cancel)?;
        allocation.map_err(|_| budget())?;
        for range in self
            .extents
            .get(location.extents.clone())
            .ok_or_else(invalid)?
        {
            for chunk in self
                .bytes
                .get(range.clone())
                .ok_or_else(invalid)?
                .chunks(64 * 1024)
            {
                super::cancelled(self.cancel)?;
                payload.extend_from_slice(chunk);
            }
        }
        if payload.len() != length {
            return Err(invalid());
        }
        super::metadata::exif_orientation(&payload, self.cancel)
    }

    fn validate(&mut self, format: PlatformFormat) -> Result<(), PlatformError> {
        let primary = self.primary.ok_or_else(unsupported)?;
        if !self
            .items
            .iter()
            .any(|item| item.id == primary && item.kind.image())
        {
            return Err(unsupported());
        }
        if self.items.len() != self.locations.len() {
            return Err(unsupported());
        }
        let mut image_bytes = 0u64;
        for location in &self.locations {
            super::cancelled(self.cancel)?;
            let item = self
                .items
                .iter()
                .find(|item| item.id == location.id)
                .ok_or_else(invalid)?;
            if item.kind.image() {
                if location.length > MAX_BYTES {
                    return Err(budget());
                }
                image_bytes = image_bytes
                    .checked_add(location.length)
                    .filter(|length| *length <= MAX_BYTES)
                    .ok_or_else(budget)?;
            } else {
                let cap = if item.kind == ItemKind::Exif {
                    256 * 1024 + 4
                } else {
                    1024 * 1024
                };
                if location.length > cap {
                    return Err(budget());
                }
                self.metadata = self
                    .metadata
                    .checked_add(location.length)
                    .filter(|value| *value <= MAX_METADATA)
                    .ok_or_else(budget)?;
            }
        }
        for extent in &self.extents {
            super::cancelled(self.cancel)?;
            if !self
                .media
                .iter()
                .any(|media| extent.start >= media.start && extent.end <= media.end)
            {
                return Err(invalid());
            }
        }
        for association in &self.associations {
            super::cancelled(self.cancel)?;
            let item = self
                .items
                .iter()
                .find(|item| item.id == association.id)
                .ok_or_else(invalid)?;
            let (mut dimensions, mut configuration) = (0, 0);
            for (position, index) in association.indexes[..association.count]
                .iter()
                .copied()
                .enumerate()
            {
                if index == 0 {
                    continue;
                }
                if association.indexes[..position].contains(&index) {
                    return Err(invalid());
                }
                let property = self
                    .properties
                    .get(usize::from(index - 1))
                    .ok_or_else(invalid)?;
                match property {
                    Property::Dimensions(_, _) => dimensions += 1,
                    Property::Av1(_) if item.kind == ItemKind::Av1 => configuration += 1,
                    Property::Hevc(_) if item.kind == ItemKind::Hevc => configuration += 1,
                    Property::Pixel(_, _, _) | Property::Color(_, _) | Property::Other(_) => {}
                    _ => return Err(unsupported()),
                }
            }
            if item.kind.image() && (dimensions != 1 || configuration != 1)
                || !item.kind.image() && (dimensions != 0 || configuration != 0)
            {
                return Err(unsupported());
            }
        }
        for item in &self.items {
            super::cancelled(self.cancel)?;
            if !self.locations.iter().any(|location| location.id == item.id) {
                return Err(unsupported());
            }
            if item.kind.image()
                && (!self
                    .associations
                    .iter()
                    .any(|association| association.id == item.id)
                    || item.kind
                        != match format {
                            PlatformFormat::Avif => ItemKind::Av1,
                            PlatformFormat::Heif => ItemKind::Hevc,
                        })
            {
                return Err(unsupported());
            }
        }
        for reference in &self.references {
            super::cancelled(self.cancel)?;
            let from = self
                .items
                .iter()
                .find(|item| item.id == reference.from)
                .ok_or_else(invalid)?;
            let to = self
                .items
                .iter()
                .find(|item| item.id == reference.to)
                .ok_or_else(invalid)?;
            if from.kind.image() || !to.kind.image() {
                return Err(unsupported());
            }
        }
        super::cancelled(self.cancel)
    }
}

fn inspect_directory<'a>(
    bytes: &'a [u8],
    max_output_bytes: u64,
    cancel: &'a AtomicBool,
) -> Result<Scan<'a>, PlatformError> {
    super::cancelled(cancel)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(budget());
    }
    let format = super::detect(bytes)?.ok_or_else(unsupported)?;
    let mut scan = Scan {
        bytes,
        cancel,
        max_output_bytes,
        boxes: 0,
        metadata: 0,
        primary: None,
        items: Vec::new(),
        properties: Vec::new(),
        associations: Vec::new(),
        locations: Vec::new(),
        extents: Vec::new(),
        media: Vec::new(),
        references: Vec::new(),
    };
    let mut position = 0;
    let first = scan.next(&mut position, bytes.len())?;
    if first.kind != *b"ftyp" {
        return Err(unsupported());
    }
    let body = scan.payload(first);
    let brands = std::iter::once(&body[..4]).chain(body[8..].chunks_exact(4));
    if brands
        .into_iter()
        .any(|brand| matches!(brand, b"avis" | b"msf1" | b"hevc" | b"hevx"))
    {
        return Err(unsupported());
    }
    scan.charge((first.end - first.start) as u64)?;
    let mut meta = false;
    while position < bytes.len() {
        let info = scan.next(&mut position, bytes.len())?;
        match &info.kind {
            b"mdat" => push(&mut scan.media, info.body..info.end, MAX_ITEMS)?,
            b"meta" if !meta => {
                scan.charge((info.end - info.start) as u64)?;
                scan.meta(info)?;
                meta = true;
            }
            b"meta" | b"ftyp" => return Err(invalid()),
            b"free" | b"skip" => scan.charge((info.end - info.start) as u64)?,
            _ => return Err(unsupported()),
        }
    }
    if !meta || scan.media.is_empty() {
        return Err(unsupported());
    }
    scan.validate(format)?;
    Ok(scan)
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ItemColor {
    pub container_cicp: Option<Cicp>,
    pub coded_color: CodedColor,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ItemInfo {
    pub item_id: u32,
    pub hidden: bool,
    pub coded: CodedInfo,
    pub orientation: Option<image::metadata::Orientation>,
    pub color: ItemColor,
}

pub(super) struct FrozenItem {
    pub bytes: Vec<u8>,
    pub item: ItemInfo,
}

struct FrozenSpec {
    kind: ItemKind,
    length: usize,
    extents: Range<usize>,
    association: Association,
    icc: Option<BoxInfo>,
}

pub(super) struct Plan<'a> {
    pub(super) items: Vec<CodedInfo>,
    pub(super) logical_bytes: u64,
    pub(super) metadata_bytes: u64,
    pub(super) picture_storage_bytes: u64,
    pub(super) largest_output_bytes: u64,
    source: &'a [u8],
    catalog: Vec<ItemInfo>,
    specs: Vec<FrozenSpec>,
    properties: Vec<Property>,
    extents: Vec<Range<usize>>,
}

impl std::fmt::Debug for Plan<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Plan")
            .field("source_bytes", &self.source.len())
            .field("catalog", &self.catalog)
            .field("logical_bytes", &self.logical_bytes)
            .field("picture_storage_bytes", &self.picture_storage_bytes)
            .field("largest_output_bytes", &self.largest_output_bytes)
            .finish_non_exhaustive()
    }
}

impl Plan<'_> {
    pub(super) fn catalog(&self) -> &[ItemInfo] {
        &self.catalog
    }

    pub(super) fn primary_index(&self) -> usize {
        0
    }

    pub(super) fn frozen_bytes(&self, index: usize) -> Result<usize, PlatformError> {
        item::frozen_bytes(self, index)
    }

    pub(super) fn freeze_item(
        &self,
        index: usize,
        cancel: &AtomicBool,
    ) -> Result<FrozenItem, PlatformError> {
        item::freeze_item(self, index, cancel)
    }

    /// 只借用该图项关联的唯一ICC，不复制、不推断有效性或授予导出保留资格。
    pub(super) fn icc_profile(&self, index: usize) -> Result<Option<&[u8]>, PlatformError> {
        let spec = self.specs.get(index).ok_or_else(invalid)?;
        let Some(info) = spec.icc else {
            return Ok(None);
        };
        let start = info.body.checked_add(4).ok_or_else(invalid)?;
        let profile = self.source.get(start..info.end).ok_or_else(invalid)?;
        if profile.is_empty() || profile.len() > 4 * 1024 * 1024 {
            return Err(budget());
        }
        Ok(Some(profile))
    }

    /// 借用已核单个HEVC图项的配置与有序码流；不带入容器或其它图片。
    pub(super) fn hevc_input(
        &self,
        index: usize,
    ) -> Result<super::hevc_native::Input<'_>, PlatformError> {
        let selected = *self.catalog.get(index).ok_or_else(invalid)?;
        let spec = self.specs.get(index).ok_or_else(invalid)?;
        if spec.kind != ItemKind::Hevc || spec.icc.is_some() {
            return Err(unsupported());
        }
        let cicp = selected.color.container_cicp.ok_or_else(unsupported)?;
        let configuration = spec.association.indexes[..spec.association.count]
            .iter()
            .filter(|index| **index != 0)
            .find_map(|index| match self.properties.get(usize::from(*index - 1)) {
                Some(Property::Hevc(info)) => Some(*info),
                _ => None,
            })
            .ok_or_else(invalid)?;
        let config = self
            .source
            .get(configuration.body..configuration.end)
            .ok_or_else(invalid)?;
        let extents = self.extents.get(spec.extents.clone()).ok_or_else(invalid)?;
        let data = ByteView::from_extents(self.source, extents)?;
        if data.len() != spec.length {
            return Err(invalid());
        }
        Ok(super::hevc_native::Input {
            config,
            data,
            coded: selected.coded,
            cicp,
        })
    }

    fn directory_bytes(&self) -> Result<u64, PlatformError> {
        let mut total = 0u64;
        for (capacity, width) in [
            (self.items.capacity(), std::mem::size_of::<CodedInfo>()),
            (self.catalog.capacity(), std::mem::size_of::<ItemInfo>()),
            (self.specs.capacity(), std::mem::size_of::<FrozenSpec>()),
            (self.properties.capacity(), std::mem::size_of::<Property>()),
            (self.extents.capacity(), std::mem::size_of::<Range<usize>>()),
        ] {
            let bytes = capacity
                .checked_mul(width)
                .and_then(|value| u64::try_from(value).ok())
                .ok_or_else(budget)?;
            total = total
                .checked_add(bytes)
                .filter(|bytes| *bytes <= MAX_BYTES)
                .ok_or_else(budget)?;
        }
        Ok(total)
    }

    /// 仅计本计划仍拥有的Vec容量；来源切片及元数据逻辑预留由调用者另计。
    pub(super) fn retained_bytes(&self) -> Result<u64, PlatformError> {
        self.directory_bytes()
    }

    /// 仅核选中图项的码流与像素模型；原来源、虚拟Vec及COM副本由调用者另计。
    pub(super) fn before_native_bytes_for(&self, index: usize) -> Result<u64, PlatformError> {
        let selected = self.catalog.get(index).ok_or_else(invalid)?;
        let spec = self.specs.get(index).ok_or_else(invalid)?;
        let output = u64::from(selected.coded.display_dimensions.0)
            .checked_mul(u64::from(selected.coded.display_dimensions.1))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(budget)?;
        let pictures = selected.coded.picture_storage_bytes()?;
        let directory = self.directory_bytes()?;
        output
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_add(pictures))
            .and_then(|bytes| bytes.checked_add(spec.length as u64))
            .and_then(|bytes| bytes.checked_add(self.metadata_bytes))
            .and_then(|bytes| bytes.checked_add(directory))
            .filter(|bytes| *bytes <= MAX_BYTES)
            .ok_or_else(budget)
    }

    /// 系统默认像素格式未知时，先以最多8字节每像素预留native目标及两份BGRA。
    #[cfg(test)]
    pub(super) fn before_native_bytes(&self) -> Result<u64, PlatformError> {
        let directory_bytes = self.directory_bytes()?;
        self.largest_output_bytes
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_add(self.picture_storage_bytes))
            .and_then(|bytes| bytes.checked_add(self.logical_bytes))
            .and_then(|bytes| bytes.checked_add(self.metadata_bytes))
            .and_then(|bytes| bytes.checked_add(directory_bytes))
            .filter(|bytes| *bytes <= MAX_BYTES)
            .ok_or_else(budget)
    }

    #[cfg(test)]
    pub(super) fn accepts_dimensions(&self, raw: (u32, u32), oriented: (u32, u32)) -> bool {
        self.items.iter().any(|info| info.display_dimensions == raw)
            && (oriented == raw || oriented == (raw.1, raw.0))
    }

    /// 仅供候选共识对照测试；生产路径使用明确绑定图项的ItemInfo。
    #[cfg(test)]
    pub(super) fn source_facts(&self, raw: (u32, u32)) -> (Option<(u32, u32)>, Option<u8>) {
        let mut matching = self
            .items
            .iter()
            .filter(|info| info.display_dimensions == raw);
        let Some(first) = matching.next() else {
            return (None, None);
        };
        let (mut dimensions, mut depth) =
            (Some(first.coded_dimensions), Some(first.bits_per_channel));
        for info in matching {
            if Some(info.coded_dimensions) != dimensions {
                dimensions = None;
            }
            if Some(info.bits_per_channel) != depth {
                depth = None;
            }
        }
        (dimensions, depth)
    }
}

#[cfg(test)]
impl Plan<'static> {
    fn test_model(
        items: Vec<CodedInfo>,
        logical_bytes: u64,
        metadata_bytes: u64,
        picture_storage_bytes: u64,
        largest_output_bytes: u64,
    ) -> Self {
        Self {
            items,
            logical_bytes,
            metadata_bytes,
            picture_storage_bytes,
            largest_output_bytes,
            source: &[],
            catalog: Vec::new(),
            specs: Vec::new(),
            properties: Vec::new(),
            extents: Vec::new(),
        }
    }
}

/// 所有系统可见图项都先检查实际编码头；不能把首个配置套到其它图项。
pub(super) fn inspect<'a>(
    bytes: &'a [u8],
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<Plan<'a>, PlatformError> {
    let scan = inspect_directory(bytes, max_output_bytes, cancel)?;
    let mut plan = Plan {
        items: Vec::new(),
        logical_bytes: 0,
        // 包含元数据图项的逻辑长度，重复extent也已由目录计入，不能只看物理源长。
        metadata_bytes: scan.metadata,
        picture_storage_bytes: 0,
        largest_output_bytes: 0,
        source: bytes,
        catalog: Vec::new(),
        specs: Vec::new(),
        properties: Vec::new(),
        extents: Vec::new(),
    };
    for item in scan.items.iter().filter(|item| item.kind.image()) {
        super::cancelled(cancel)?;
        let location = scan
            .locations
            .iter()
            .find(|location| location.id == item.id)
            .ok_or_else(invalid)?;
        let association = scan
            .associations
            .iter()
            .find(|association| association.id == item.id)
            .ok_or_else(invalid)?;
        let (mut dimensions, mut configuration, mut pixel) = (None, None, None);
        let mut container_cicp = None;
        let mut icc = None;
        for index in &association.indexes[..association.count] {
            if *index == 0 {
                continue;
            }
            match *scan
                .properties
                .get(usize::from(*index - 1))
                .ok_or_else(invalid)?
            {
                Property::Dimensions(value, _) => dimensions = Some(value),
                Property::Av1(value) | Property::Hevc(value) => configuration = Some(value),
                Property::Pixel(channels, bits, _) => {
                    if pixel.replace((channels, bits)).is_some() {
                        return Err(invalid());
                    }
                }
                Property::Color(ColorProperty::Nclx(cicp), _) => {
                    if container_cicp.replace(cicp).is_some() {
                        return Err(invalid());
                    }
                }
                Property::Color(ColorProperty::Icc, info) => {
                    if icc.replace(info).is_some() {
                        return Err(coded::unsupported(
                            "图项关联了多份ICC配置，暂不支持",
                            "Multiple ICC profiles associated with an item are unsupported",
                        ));
                    }
                }
                Property::Other(_) => {}
            }
        }
        let declared = dimensions.ok_or_else(unsupported)?;
        let configuration = configuration.ok_or_else(unsupported)?;
        let config = scan.payload(configuration);
        let extents = scan
            .extents
            .get(location.extents.clone())
            .ok_or_else(invalid)?;
        let data = ByteView::from_extents(bytes, extents)?;
        if data.len() as u64 != location.length {
            return Err(invalid());
        }
        let info = match item.kind {
            ItemKind::Av1 => coded::inspect_av1(config, data, declared, max_output_bytes, cancel)?,
            ItemKind::Hevc => {
                coded::inspect_hevc(config, data, declared, max_output_bytes, cancel)?
            }
            _ => return Err(invalid()),
        };
        if info.display_dimensions != declared {
            return Err(invalid());
        }
        if info.color.model == SampleModel::Rgb && container_cicp.is_none() && icc.is_none() {
            return Err(coded::unsupported(
                "RGB图项缺少颜色声明，暂不支持",
                "RGB image items without a color declaration are unsupported",
            ));
        }
        if let Some((channels, bits)) = pixel {
            let expected = if item.kind == ItemKind::Av1 && config[2] & 0x10 != 0 {
                1
            } else {
                3
            };
            if channels != expected
                || bits[..usize::from(channels)]
                    .iter()
                    .any(|bits| *bits != info.bits_per_channel)
            {
                return Err(invalid());
            }
        }
        plan.logical_bytes = plan
            .logical_bytes
            .checked_add(location.length)
            .filter(|bytes| *bytes <= MAX_BYTES)
            .ok_or_else(budget)?;
        plan.picture_storage_bytes = plan
            .picture_storage_bytes
            .checked_add(info.picture_storage_bytes()?)
            .ok_or_else(budget)?;
        let output =
            u64::from(info.display_dimensions.0) * u64::from(info.display_dimensions.1) * 4;
        plan.largest_output_bytes = plan.largest_output_bytes.max(output);
        push(&mut plan.items, info, MAX_ITEMS)?;
        let orientation = scan.item_orientation(item.id)?;
        if !item.hidden || Some(item.id) == scan.primary {
            push(
                &mut plan.catalog,
                ItemInfo {
                    item_id: item.id,
                    hidden: item.hidden,
                    coded: info,
                    orientation,
                    color: ItemColor {
                        container_cicp,
                        coded_color: info.color,
                    },
                },
                MAX_ITEMS,
            )?;
            push(
                &mut plan.specs,
                FrozenSpec {
                    kind: item.kind,
                    length: usize::try_from(location.length).map_err(|_| budget())?,
                    extents: location.extents.clone(),
                    association: *association,
                    icc,
                },
                MAX_ITEMS,
            )?;
        }
    }
    if plan.items.is_empty() {
        return Err(unsupported());
    }
    let primary = scan.primary.ok_or_else(invalid)?;
    let primary = plan
        .catalog
        .iter()
        .position(|entry| entry.item_id == primary)
        .ok_or_else(invalid)?;
    plan.catalog[..=primary].rotate_right(1);
    plan.specs[..=primary].rotate_right(1);
    plan.properties = scan.properties;
    plan.extents = scan.extents;
    for index in 0..plan.catalog.len() {
        super::cancelled(cancel)?;
        let result = plan.before_native_bytes_for(index);
        super::cancelled(cancel)?;
        result?;
    }
    super::cancelled(cancel)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 以下目录模型的mdat不是可解码图像，只验证目录；完整入口另用真实编码样本。
    fn inspect(
        bytes: &[u8],
        max_output_bytes: u64,
        cancel: &AtomicBool,
    ) -> Result<(), PlatformError> {
        super::inspect_directory(bytes, max_output_bytes, cancel).map(|_| ())
    }

    #[test]
    fn image_platform_preflight_actual_coded_heads_keep_canvas_crop_and_precision_distinct() {
        let cancel = token();
        let white = include_bytes!("../tests/data/image_platform/white_1x1.avif");
        let plan = super::inspect(white, 4, &cancel).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].coded_dimensions, (1, 1));
        assert_eq!(plan.items[0].display_dimensions, (1, 1));
        assert_eq!(plan.items[0].bits_per_channel, 8);
        assert_eq!(plan.items[0].picture_buffers, 1);
        assert_eq!(plan.logical_bytes, 23);
        assert_eq!(plan.picture_storage_bytes, 4);
        let heic = include_bytes!("../tests/data/image_platform/hevc-confwin-420-8bit-left.heic");
        let plan = super::inspect(heic, 250 * 128 * 4, &cancel).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].coded_dimensions, (256, 128));
        assert_eq!(plan.items[0].display_dimensions, (250, 128));
        assert_eq!(plan.items[0].bits_per_channel, 8);
        assert_eq!(plan.picture_storage_bytes, 256 * 128 * 4);
        assert_eq!(plan.source_facts((250, 128)), (Some((256, 128)), Some(8)));
        assert!(plan.accepts_dimensions((250, 128), (128, 250)));
        assert!(!plan.accepts_dimensions((256, 128), (256, 128)));
    }

    #[test]
    fn image_platform_preflight_original_hdr_auxiliary_container_still_requires_support() {
        let source = include_bytes!("../tests/data/image_platform/colors_hdr_rec2020.avif");
        assert!(matches!(
            super::inspect(source, MAX_BYTES, &token()),
            Err(PlatformError::Unsupported { .. })
        ));
    }

    #[test]
    fn image_platform_preflight_wrong_associated_ispe_and_invalid_coded_payload_are_rejected() {
        let original = include_bytes!("../tests/data/image_platform/white_1x1.avif");
        let cancel = token();
        let directory = super::inspect_directory(original, MAX_BYTES, &cancel).unwrap();
        let ispe = original
            .windows(4)
            .position(|bytes| bytes == b"ispe")
            .unwrap();
        let mut changed = original.to_vec();
        changed[ispe + 8..ispe + 12].copy_from_slice(&2u32.to_be_bytes());
        assert!(super::inspect_directory(&changed, MAX_BYTES, &cancel).is_ok());
        assert!(super::inspect(&changed, MAX_BYTES, &cancel).is_err());
        let location = directory.locations.first().unwrap();
        let extent = directory.extents[location.extents.start].clone();
        let mut invalid = original.to_vec();
        invalid[extent.start] = 0xff;
        assert!(super::inspect_directory(&invalid, MAX_BYTES, &cancel).is_ok());
        assert!(super::inspect(&invalid, MAX_BYTES, &cancel).is_err());
    }

    #[test]
    fn image_platform_preflight_ambiguous_same_size_facts_remain_unknown() {
        let first = CodedInfo {
            coded_dimensions: (256, 128),
            display_dimensions: (250, 128),
            display_origin: (6, 0),
            bits_per_channel: 8,
            picture_buffers: 1,
            color: CodedColor::UNKNOWN,
        };
        let mut plan = Plan::test_model(
            vec![
                first,
                CodedInfo {
                    coded_dimensions: (258, 128),
                    bits_per_channel: 10,
                    ..first
                },
            ],
            0,
            0,
            0,
            0,
        );
        assert_eq!(plan.source_facts((250, 128)), (None, None));
        assert_eq!(plan.source_facts((17, 19)), (None, None));
        plan.items.pop();
        assert_eq!(plan.source_facts((250, 128)), (Some((256, 128)), Some(8)));
        plan.items.push(first);
        assert_eq!(plan.source_facts((250, 128)), (Some((256, 128)), Some(8)));
    }

    #[test]
    fn image_platform_preflight_validates_non_primary_coded_items_and_counts_shared_payloads() {
        let white = include_bytes!("../tests/data/image_platform/white_1x1.avif");
        let cancel = token();
        let source = super::inspect_directory(white, MAX_BYTES, &cancel).unwrap();
        let source_item = &source.locations[0];
        let payload = &white[source.extents[source_item.extents.start].clone()];
        let mut bytes = fixture_layout(false, 0, 2, 1, payload.len());
        let directory = super::inspect_directory(&bytes, MAX_BYTES, &cancel).unwrap();
        let target = directory.extents[0].clone();
        drop(directory);
        bytes[target].copy_from_slice(payload);
        let ispe = bytes.windows(4).position(|value| value == b"ispe").unwrap();
        bytes[ispe + 8..ispe + 12].copy_from_slice(&1u32.to_be_bytes());
        bytes[ispe + 12..ispe + 16].copy_from_slice(&1u32.to_be_bytes());
        let config = bytes.windows(4).position(|value| value == b"av1C").unwrap();
        bytes[config + 4..config + 8].copy_from_slice(&[0x81, 0x20, 0, 0]);
        let plan = super::inspect(&bytes, 4, &cancel).unwrap();
        assert_eq!(plan.items.len(), 2);
        // 两个图项共享物理extent，逻辑码流与picture模型仍各自计入。
        assert_eq!(plan.logical_bytes, 2 * payload.len() as u64);
        assert_eq!(plan.picture_storage_bytes, 8);
        let invalid_offset = bytes.len() + 8;
        let mut invalid_payload = payload.to_vec();
        invalid_payload[0] = 0xff;
        bytes.extend_from_slice(&boxed(b"mdat", &invalid_payload, false));
        let iloc = bytes.windows(4).position(|value| value == b"iloc").unwrap();
        // 固定fixture版本0：full4+sizes2+count2+首项14+第二项头6。
        let second_offset = iloc + 4 + 4 + 2 + 2 + 14 + 6;
        bytes[second_offset..second_offset + 4]
            .copy_from_slice(&(invalid_offset as u32).to_be_bytes());
        assert!(super::inspect_directory(&bytes, MAX_BYTES, &cancel).is_ok());
        // 首项未变且可以通过头检查，完整入口仍须拒绝非主图的坏码流头。
        assert!(super::inspect(&bytes, MAX_BYTES, &cancel).is_err());
    }

    fn boxed(kind: &[u8; 4], payload: &[u8], extended: bool) -> Vec<u8> {
        let mut result = Vec::new();
        if extended {
            result.extend_from_slice(&1u32.to_be_bytes());
            result.extend_from_slice(kind);
            result.extend_from_slice(&(payload.len() as u64 + 16).to_be_bytes());
        } else {
            result.extend_from_slice(&(payload.len() as u32 + 8).to_be_bytes());
            result.extend_from_slice(kind);
        }
        result.extend_from_slice(payload);
        result
    }
    fn full(version: u8, data: &[u8]) -> Vec<u8> {
        let mut result = vec![version, 0, 0, 0];
        result.extend_from_slice(data);
        result
    }
    // 自造目录验证声明与边界；mdat为占位字节，不用于证明任何像素可解码。
    fn fixture(extended: bool) -> Vec<u8> {
        fixture_version(extended, 0)
    }
    fn fixture_version(extended: bool, version: u8) -> Vec<u8> {
        fixture_layout(extended, version, 1, 1, 3)
    }
    fn fixture_layout(
        extended: bool,
        version: u8,
        items: u16,
        extent_count: u16,
        payload_size: usize,
    ) -> Vec<u8> {
        let wide = version == 2;
        let id = |index: u16| {
            if wide {
                (65_536 + u32::from(index)).to_be_bytes().to_vec()
            } else {
                index.to_be_bytes().to_vec()
            }
        };
        let count = if wide {
            u32::from(items).to_be_bytes().to_vec()
        } else {
            items.to_be_bytes().to_vec()
        };
        let mut data = boxed(b"ftyp", b"avif\0\0\0\0mif1", extended);
        let payload_offset = data.len() + if extended { 16 } else { 8 };
        data.extend_from_slice(&boxed(b"mdat", &vec![1; payload_size], extended));
        let mut hdlr = vec![0; 4];
        hdlr.extend_from_slice(b"pict");
        hdlr.extend_from_slice(&[0; 13]);
        let mut meta = boxed(b"hdlr", &full(0, &hdlr), extended);
        meta.extend_from_slice(&boxed(b"pitm", &full(u8::from(wide), &id(1)), extended));
        let mut iinf = count.clone();
        for index in 1..=items {
            let mut entry = id(index);
            entry.extend_from_slice(b"\0\0av01\0");
            iinf.extend_from_slice(&boxed(
                b"infe",
                &full(if wide { 3 } else { 2 }, &entry),
                extended,
            ));
        }
        meta.extend_from_slice(&boxed(b"iinf", &full(u8::from(wide), &iinf), extended));
        let mut iloc = vec![0x44, 0];
        iloc.extend_from_slice(&count);
        for index in 1..=items {
            iloc.extend_from_slice(&id(index));
            if version > 0 {
                iloc.extend_from_slice(&0u16.to_be_bytes());
            }
            iloc.extend_from_slice(&0u16.to_be_bytes());
            iloc.extend_from_slice(&extent_count.to_be_bytes());
            for _ in 0..extent_count {
                iloc.extend_from_slice(&(payload_offset as u32).to_be_bytes());
                iloc.extend_from_slice(&(payload_size as u32).to_be_bytes());
            }
        }
        meta.extend_from_slice(&boxed(b"iloc", &full(version, &iloc), extended));
        let mut ispe = 3u32.to_be_bytes().to_vec();
        ispe.extend_from_slice(&2u32.to_be_bytes());
        let mut ipco = boxed(b"ispe", &full(0, &ispe), extended);
        ipco.extend_from_slice(&boxed(b"av1C", &[0x81, 0, 0, 0], extended));
        ipco.extend_from_slice(&boxed(b"colr", b"nclx\0\x01\0\x0d\0\x06\x80", extended));
        let mut iprp = boxed(b"ipco", &ipco, extended);
        let mut associations = u32::from(items).to_be_bytes().to_vec();
        for index in 1..=items {
            associations.extend_from_slice(&id(index));
            associations.push(3);
            associations.extend_from_slice(if wide {
                &[0, 1, 0x80, 2, 0, 3][..]
            } else {
                &[1, 0x82, 3][..]
            });
        }
        let mut ipma = full(u8::from(wide), &associations);
        ipma[3] = u8::from(wide);
        iprp.extend_from_slice(&boxed(b"ipma", &ipma, extended));
        meta.extend_from_slice(&boxed(b"iprp", &iprp, extended));
        data.extend_from_slice(&boxed(b"meta", &full(0, &meta), extended));
        data
    }
    fn locate(bytes: &[u8], kind: &[u8; 4]) -> usize {
        bytes.windows(4).position(|window| window == kind).unwrap()
    }
    fn token() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn image_platform_preflight_accepts_fixed_static_directory_and_exact_output_budget() {
        for extended in [false, true] {
            let bytes = fixture(extended);
            assert_eq!(inspect(&bytes, 24, &token()), Ok(()));
            assert!(matches!(
                inspect(&bytes, 23, &token()),
                Err(PlatformError::Budget(_))
            ));
        }
    }
    #[test]
    fn image_platform_preflight_accepts_real_still_container_declarations_without_decoding() {
        let avif = include_bytes!("../tests/data/image_platform/white_1x1.avif");
        assert_eq!(inspect(avif, 4, &token()), Ok(()));
        let heic = include_bytes!("../tests/data/image_platform/hevc-confwin-420-8bit-left.heic");
        assert_eq!(inspect(heic, 250 * 128 * 4, &token()), Ok(()));
        assert!(matches!(
            inspect(heic, 250 * 128 * 4 - 1, &token()),
            Err(PlatformError::Budget(_))
        ));
    }
    #[test]
    fn image_platform_preflight_checks_location_versions_and_wide_associations() {
        for version in [1, 2] {
            let mut bytes = fixture_version(false, version);
            assert_eq!(inspect(&bytes, 24, &token()), Ok(()));
            let sizes = locate(&bytes, b"iloc") + 8;
            let method = sizes + 2 + if version == 2 { 8 } else { 4 };
            bytes[method..method + 2].copy_from_slice(&0x1000u16.to_be_bytes());
            assert!(matches!(
                inspect(&bytes, MAX_BYTES, &token()),
                Err(PlatformError::Unsupported { .. })
            ));
        }
    }
    #[test]
    fn image_platform_preflight_limits_logical_streams_while_allowing_bounded_shared_extents() {
        let payload = 4 * 1024 * 1024;
        assert_eq!(
            inspect(&fixture_layout(false, 0, 1, 64, payload), 24, &token()),
            Ok(())
        );
        assert!(matches!(
            inspect(&fixture_layout(false, 0, 1, 64, payload + 1), 24, &token()),
            Err(PlatformError::Budget(_))
        ));
        // 每项128MiB本身有界，但两项共享载荷重组后的总长仍须计入上限。
        assert!(matches!(
            inspect(&fixture_layout(false, 0, 2, 32, payload + 1), 24, &token()),
            Err(PlatformError::Budget(_))
        ));
    }
    #[test]
    fn image_platform_preflight_rejects_large_ispe_even_when_not_associated() {
        let mut bytes = fixture(false);
        let width = locate(&bytes, b"ispe") + 8;
        bytes[width..width + 4].copy_from_slice(&65_536u32.to_be_bytes());
        assert!(matches!(
            inspect(&bytes, MAX_BYTES, &token()),
            Err(PlatformError::Budget(_))
        ));
        let mut bytes = fixture(false);
        let association = locate(&bytes, b"ipma") + 4 + 4 + 4 + 2 + 1;
        bytes[association] = 0;
        let width = locate(&bytes, b"ispe") + 8;
        bytes[width..width + 4].copy_from_slice(&8001u32.to_be_bytes());
        bytes[width + 4..width + 8].copy_from_slice(&4000u32.to_be_bytes());
        assert!(matches!(
            inspect(&bytes, MAX_BYTES, &token()),
            Err(PlatformError::Budget(_))
        ));
    }
    #[test]
    fn image_platform_preflight_rejects_missing_associations_unknown_properties_and_derived_items()
    {
        for replacement in [*b"grid", *b"iovl", *b"iden"] {
            let mut bytes = fixture(false);
            let kind = locate(&bytes, b"av01");
            bytes[kind..kind + 4].copy_from_slice(&replacement);
            assert!(matches!(
                inspect(&bytes, MAX_BYTES, &token()),
                Err(PlatformError::Unsupported { .. })
            ));
        }
        for replacement in [*b"lsel", *b"a1op", *b"clap", *b"zzzz"] {
            let mut bytes = fixture(false);
            let kind = locate(&bytes, b"av1C");
            bytes[kind..kind + 4].copy_from_slice(&replacement);
            assert!(matches!(
                inspect(&bytes, MAX_BYTES, &token()),
                Err(PlatformError::Unsupported { .. })
            ));
        }
        let mut bytes = fixture(false);
        let index = locate(&bytes, b"ipma") + 4 + 4 + 4 + 2 + 1;
        bytes[index] = 0;
        assert!(matches!(
            inspect(&bytes, MAX_BYTES, &token()),
            Err(PlatformError::Unsupported { .. })
        ));
        bytes[index] = 127;
        assert!(matches!(
            inspect(&bytes, MAX_BYTES, &token()),
            Err(PlatformError::Invalid { .. })
        ));
    }
    #[test]
    fn image_platform_preflight_checks_container_ranges_locations_and_external_or_sequence_data() {
        let original = fixture(false);
        for length in 20..original.len() {
            assert!(inspect(&original[..length], MAX_BYTES, &token()).is_err());
        }
        let mut bytes = original.clone();
        let child = locate(&bytes, b"ipco") - 4;
        bytes[child..child + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            inspect(&bytes, MAX_BYTES, &token()),
            Err(PlatformError::Invalid { .. })
        ));
        let mut bytes = original.clone();
        let iloc = locate(&bytes, b"iloc") + 4 + 4;
        bytes[iloc + 6..iloc + 8].copy_from_slice(&1u16.to_be_bytes());
        assert!(matches!(
            inspect(&bytes, MAX_BYTES, &token()),
            Err(PlatformError::Unsupported { .. })
        ));
        let mut bytes = original.clone();
        let iloc = locate(&bytes, b"iloc") + 4 + 4;
        bytes[iloc + 10..iloc + 14].copy_from_slice(&0u32.to_be_bytes());
        assert!(matches!(
            inspect(&bytes, MAX_BYTES, &token()),
            Err(PlatformError::Invalid { .. })
        ));
        for kind in [b"moov", b"uuid"] {
            let mut bytes = original.clone();
            bytes.extend_from_slice(&boxed(kind, &[], false));
            assert!(matches!(
                inspect(&bytes, MAX_BYTES, &token()),
                Err(PlatformError::Unsupported { .. })
            ));
        }
        let mut bytes = original;
        bytes[8..12].copy_from_slice(b"avis");
        assert!(matches!(
            inspect(&bytes, MAX_BYTES, &token()),
            Err(PlatformError::Unsupported { .. })
        ));
    }
    #[test]
    fn image_platform_preflight_enforces_box_metadata_and_cancellation_limits() {
        let mut bytes = fixture(false);
        for _ in 0..MAX_BOXES {
            bytes.extend_from_slice(&boxed(b"free", &[], false));
        }
        assert!(matches!(
            inspect(&bytes, MAX_BYTES, &token()),
            Err(PlatformError::Budget(_))
        ));
        let mut bytes = fixture(false);
        bytes.extend_from_slice(&boxed(b"free", &vec![0; MAX_METADATA as usize], false));
        assert!(matches!(
            inspect(&bytes, MAX_BYTES, &token()),
            Err(PlatformError::Budget(_))
        ));
        let cancel = AtomicBool::new(true);
        assert_eq!(
            inspect(&fixture(false), MAX_BYTES, &cancel),
            Err(PlatformError::Cancelled)
        );
    }
}
