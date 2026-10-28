//! 只重封已预检的单个独立图项；不复制其它图片或原始EXIF/XMP。
use super::{
    budget, invalid, Association, FrozenItem, FrozenSpec, ItemKind, Plan, Property, MAX_BYTES,
};
use std::sync::atomic::AtomicBool;

struct Layout {
    total: usize,
    meta: usize,
    iprp: usize,
    ipco: usize,
    ipma: usize,
    properties: usize,
    payload: usize,
    ftyp: usize,
    generated_pixel: Option<u8>,
    brand: [u8; 4],
}

fn add(left: usize, right: usize) -> Result<usize, super::PlatformError> {
    left.checked_add(right)
        .filter(|value| *value as u64 <= MAX_BYTES)
        .ok_or_else(budget)
}

fn retained(property: Property) -> bool {
    // 来源只接收恒等irot与1:1 pasp；重封省略它们，不引入显示变换或比例。
    !matches!(&property.info().kind, b"irot" | b"pasp")
}

fn layout(plan: &Plan<'_>, index: usize) -> Result<Layout, super::PlatformError> {
    plan.catalog.get(index).ok_or_else(invalid)?;
    let spec = plan.specs.get(index).ok_or_else(invalid)?;
    let mut property_bytes = 0usize;
    let mut properties = 0usize;
    let mut configuration = None;
    let mut pixel = false;
    for index in &spec.association.indexes[..spec.association.count] {
        if *index == 0 {
            continue;
        }
        let property = *plan
            .properties
            .get(usize::from(*index - 1))
            .ok_or_else(invalid)?;
        if !retained(property) {
            continue;
        }
        let info = property.info();
        let bytes = plan.source.get(info.start..info.end).ok_or_else(invalid)?;
        property_bytes = add(property_bytes, bytes.len())?;
        properties += 1;
        if matches!(property, Property::Av1(_) | Property::Hevc(_)) {
            configuration = Some(info);
        }
        pixel |= matches!(property, Property::Pixel(_, _, _));
    }
    let generated_pixel = if spec.kind == ItemKind::Av1 && !pixel {
        let config = configuration.ok_or_else(invalid)?;
        let mono = plan
            .source
            .get(config.body + 2)
            .copied()
            .ok_or_else(invalid)?
            & 0x10
            != 0;
        let channels = if mono { 1u8 } else { 3 };
        property_bytes = add(property_bytes, 13 + usize::from(channels))?;
        properties += 1;
        Some(channels)
    } else {
        None
    };
    if properties == 0 {
        return Err(invalid());
    }
    if properties > super::MAX_ASSOCIATIONS {
        return Err(budget());
    }
    let brand = match spec.kind {
        ItemKind::Av1 => *b"avif",
        ItemKind::Hevc => {
            let config = configuration.ok_or_else(invalid)?;
            // 实际SPS/VPS已经与hvcC的PTL完全绑定；不复制原文件其它能力品牌。
            let profile = plan
                .source
                .get(config.body + 1)
                .copied()
                .ok_or_else(invalid)?
                & 31;
            match profile {
                1 | 3 => *b"heic",
                2 => *b"heix",
                _ => return Err(invalid()),
            }
        }
        _ => return Err(invalid()),
    };
    let mut logical = 0usize;
    for extent in plan.extents.get(spec.extents.clone()).ok_or_else(invalid)? {
        let bytes = plan.source.get(extent.clone()).ok_or_else(invalid)?;
        logical = add(logical, bytes.len())?;
    }
    if logical == 0 || logical != spec.length {
        return Err(invalid());
    }
    let ipco = add(8, property_bytes)?;
    // 版本0的一项ipma：fullbox4+count4+itemID2+associationCount1。
    let ipma = add(19, properties)?;
    let iprp = add(8, add(ipco, ipma)?)?;
    // meta fullbox12 + hdlr33 + pitm14 + iinf35 + iloc30。
    let meta = add(124, iprp)?;
    let ftyp = if spec.kind == ItemKind::Av1 { 28 } else { 24 };
    let payload = add(add(ftyp, meta)?, 8)?;
    let total = add(payload, logical)?;
    Ok(Layout {
        total,
        meta,
        iprp,
        ipco,
        ipma,
        properties,
        payload,
        ftyp,
        generated_pixel,
        brand,
    })
}

pub(super) fn frozen_bytes(plan: &Plan<'_>, index: usize) -> Result<usize, super::PlatformError> {
    Ok(layout(plan, index)?.total)
}

struct Writer<'a> {
    bytes: Vec<u8>,
    total: usize,
    cancel: &'a AtomicBool,
}

impl Writer<'_> {
    fn append(&mut self, bytes: &[u8]) -> Result<(), super::PlatformError> {
        super::super::cancelled(self.cancel)?;
        let end = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|end| *end <= self.total)
            .ok_or_else(invalid)?;
        if end > self.bytes.capacity() {
            return Err(budget());
        }
        for chunk in bytes.chunks(64 * 1024) {
            super::super::cancelled(self.cancel)?;
            self.bytes.extend_from_slice(chunk);
        }
        super::super::cancelled(self.cancel)
    }
    fn u32(&mut self, value: usize) -> Result<(), super::PlatformError> {
        let value = u32::try_from(value).map_err(|_| budget())?;
        self.append(&value.to_be_bytes())
    }
    fn box_header(&mut self, kind: &[u8; 4], size: usize) -> Result<(), super::PlatformError> {
        self.u32(size)?;
        self.append(kind)
    }
    fn properties(
        &mut self,
        plan: &Plan<'_>,
        association: &Association,
    ) -> Result<(), super::PlatformError> {
        for index in &association.indexes[..association.count] {
            if *index == 0 {
                continue;
            }
            let property = *plan
                .properties
                .get(usize::from(*index - 1))
                .ok_or_else(invalid)?;
            if !retained(property) {
                continue;
            }
            let info = property.info();
            self.append(plan.source.get(info.start..info.end).ok_or_else(invalid)?)?;
        }
        Ok(())
    }
    fn associations(
        &mut self,
        plan: &Plan<'_>,
        association: &Association,
    ) -> Result<(), super::PlatformError> {
        let mut dense = 0u8;
        for position in 0..association.count {
            if association.indexes[position] == 0 {
                continue;
            }
            let property = *plan
                .properties
                .get(usize::from(association.indexes[position] - 1))
                .ok_or_else(invalid)?;
            if !retained(property) {
                continue;
            }
            dense = dense
                .checked_add(1)
                .filter(|value| *value < 128)
                .ok_or_else(invalid)?;
            self.append(&[dense
                | if association.essential[position] {
                    0x80
                } else {
                    0
                }])?;
        }
        Ok(())
    }
    fn payload(&mut self, plan: &Plan<'_>, spec: &FrozenSpec) -> Result<(), super::PlatformError> {
        for extent in plan.extents.get(spec.extents.clone()).ok_or_else(invalid)? {
            self.append(plan.source.get(extent.clone()).ok_or_else(invalid)?)?;
        }
        Ok(())
    }
}

pub(super) fn freeze_item(
    plan: &Plan<'_>,
    index: usize,
    cancel: &AtomicBool,
) -> Result<FrozenItem, super::PlatformError> {
    super::super::cancelled(cancel)?;
    let result = layout(plan, index);
    super::super::cancelled(cancel)?;
    let layout = result?;
    let item = *plan.catalog.get(index).ok_or_else(invalid)?;
    let spec = plan.specs.get(index).ok_or_else(invalid)?;
    let mut bytes = Vec::new();
    let allocation = bytes.try_reserve_exact(layout.total);
    super::super::cancelled(cancel)?;
    allocation.map_err(|_| budget())?;
    if bytes.capacity() as u64 > MAX_BYTES {
        return Err(budget());
    }
    let mut writer = Writer {
        bytes,
        total: layout.total,
        cancel,
    };
    writer.box_header(b"ftyp", layout.ftyp)?;
    writer.append(&layout.brand)?;
    writer.u32(0)?;
    writer.append(&layout.brand)?;
    writer.append(b"mif1")?;
    if spec.kind == ItemKind::Av1 {
        // AVIF要求MIAF；单图重封不声称任何MA1A/MA1B可选profile。
        writer.append(b"miaf")?;
    }
    writer.box_header(b"meta", layout.meta)?;
    writer.u32(0)?;
    writer.box_header(b"hdlr", 33)?;
    writer.u32(0)?;
    writer.u32(0)?;
    writer.append(b"pict")?;
    writer.append(&[0; 13])?;
    writer.box_header(b"pitm", 14)?;
    writer.u32(0)?;
    writer.append(&[0, 1])?;
    writer.box_header(b"iinf", 35)?;
    writer.u32(0)?;
    writer.append(&[0, 1])?;
    writer.box_header(b"infe", 21)?;
    writer.u32(2 << 24)?; // 唯一虚拟图项可见；源hidden仅保留在ItemInfo中。
    writer.append(&[0, 1, 0, 0])?;
    writer.append(match spec.kind {
        ItemKind::Av1 => b"av01",
        ItemKind::Hevc => b"hvc1",
        _ => return Err(invalid()),
    })?;
    writer.append(&[0])?;
    writer.box_header(b"iloc", 30)?;
    writer.u32(0)?;
    writer.append(&[0x44, 0, 0, 1, 0, 1, 0, 0, 0, 1])?;
    writer.u32(layout.payload)?;
    writer.u32(spec.length)?;
    writer.box_header(b"iprp", layout.iprp)?;
    writer.box_header(b"ipco", layout.ipco)?;
    writer.properties(plan, &spec.association)?;
    if let Some(channels) = layout.generated_pixel {
        writer.box_header(b"pixi", 13 + usize::from(channels))?;
        writer.u32(0)?;
        writer.append(&[channels])?;
        writer.append(&[item.coded.bits_per_channel; 3][..usize::from(channels)])?;
    }
    writer.box_header(b"ipma", layout.ipma)?;
    writer.u32(0)?;
    writer.u32(1)?;
    writer.append(&[0, 1, layout.properties as u8])?;
    writer.associations(plan, &spec.association)?;
    if layout.generated_pixel.is_some() {
        writer.append(&[layout.properties as u8])?;
    }
    writer.box_header(b"mdat", add(8, spec.length)?)?;
    if writer.bytes.len() != layout.payload {
        return Err(invalid());
    }
    writer.payload(plan, spec)?;
    if writer.bytes.len() != layout.total {
        return Err(invalid());
    }
    super::super::cancelled(cancel)?;
    Ok(FrozenItem {
        bytes: writer.bytes,
        item,
    })
}

#[cfg(test)]
mod tests {
    use super::super::{
        coded::{Cicp, SampleModel},
        inspect, inspect_directory, ColorProperty, ItemKind,
    };
    use super::*;

    struct Metadata {
        id: u16,
        kind: [u8; 4],
        target: Option<u16>,
        payload: Vec<u8>,
    }

    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }
    fn full(version: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![version, 0, 0, 0];
        bytes.extend_from_slice(payload);
        bytes
    }
    fn white_payload(marker: u8) -> Vec<u8> {
        let sample = include_bytes!("../tests/data/image_platform/white_1x1.avif");
        let start = sample
            .windows(4)
            .position(|bytes| bytes == b"mdat")
            .unwrap()
            + 4;
        let mut payload = sample[start..].to_vec();
        // 合法padding区分码流来源，不把控制头检查当作像素颜色测试。
        payload.extend_from_slice(&[0x7a, 1, marker]);
        payload
    }
    fn exif(id: u16, target: Option<u16>, orientation: u8) -> Metadata {
        let mut payload = 0u32.to_be_bytes().to_vec();
        payload.extend_from_slice(b"II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0");
        payload.extend_from_slice(&[orientation, 0, 0, 0]);
        payload.extend_from_slice(&[0; 4]);
        Metadata {
            id,
            kind: *b"Exif",
            target,
            payload,
        }
    }
    fn entry(id: u16, kind: &[u8; 4], hidden: bool) -> Vec<u8> {
        let mut body = id.to_be_bytes().to_vec();
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(kind);
        body.push(0);
        if kind == b"mime" {
            body.extend_from_slice(b"application/rdf+xml\0\0");
        }
        let mut body = full(2, &body);
        body[3] = u8::from(hidden);
        boxed(b"infe", &body)
    }
    fn fixture(
        images: &[(u16, bool, u8)],
        primary: u16,
        metadata: &[Metadata],
        pixi: bool,
    ) -> Vec<u8> {
        fixture_colors(images, primary, metadata, pixi, &[], &[])
    }
    fn fixture_colors(
        images: &[(u16, bool, u8)],
        primary: u16,
        metadata: &[Metadata],
        pixi: bool,
        extra_colors: &[Vec<u8>],
        color_indexes: &[(u16, Vec<u8>)],
    ) -> Vec<u8> {
        let mut bytes = boxed(b"ftyp", b"avif\0\0\0\0avifmif1miafMA1A");
        let base = bytes.len() + 8;
        let mut media = Vec::new();
        let mut locations = Vec::new();
        for (id, _, marker) in images {
            let payload = white_payload(*marker);
            let split = payload.len() / 2;
            // 物理存储后半在先；iloc逻辑顺序必须仍是先前半、后半。
            let second = base + media.len();
            media.extend_from_slice(&payload[split..]);
            let first = base + media.len();
            media.extend_from_slice(&payload[..split]);
            locations.push((
                *id,
                vec![
                    (first as u32, split as u32),
                    (second as u32, (payload.len() - split) as u32),
                ],
            ));
        }
        for item in metadata {
            let offset = base + media.len();
            media.extend_from_slice(&item.payload);
            locations.push((item.id, vec![(offset as u32, item.payload.len() as u32)]));
        }
        bytes.extend_from_slice(&boxed(b"mdat", &media));
        let mut hdlr = vec![0; 4];
        hdlr.extend_from_slice(b"pict");
        hdlr.extend_from_slice(&[0; 13]);
        let mut meta = boxed(b"hdlr", &full(0, &hdlr));
        meta.extend_from_slice(&boxed(b"pitm", &full(0, &primary.to_be_bytes())));
        let mut iinf = ((images.len() + metadata.len()) as u16)
            .to_be_bytes()
            .to_vec();
        for (id, hidden, _) in images {
            iinf.extend_from_slice(&entry(*id, b"av01", *hidden));
        }
        for item in metadata {
            iinf.extend_from_slice(&entry(item.id, &item.kind, false));
        }
        meta.extend_from_slice(&boxed(b"iinf", &full(0, &iinf)));
        let mut iloc = vec![0x44, 0];
        iloc.extend_from_slice(&(locations.len() as u16).to_be_bytes());
        for (id, extents) in locations {
            iloc.extend_from_slice(&id.to_be_bytes());
            iloc.extend_from_slice(&[0, 0]);
            iloc.extend_from_slice(&(extents.len() as u16).to_be_bytes());
            for (offset, length) in extents {
                iloc.extend_from_slice(&offset.to_be_bytes());
                iloc.extend_from_slice(&length.to_be_bytes());
            }
        }
        meta.extend_from_slice(&boxed(b"iloc", &full(0, &iloc)));
        let mut colr = b"nclx".to_vec();
        colr.extend_from_slice(&[0, 1, 0, 13, 0, 6, 0x80]);
        let mut ipco = boxed(b"colr", &colr);
        ipco.extend_from_slice(&boxed(b"av1C", &[0x81, 0x20, 0, 0]));
        ipco.extend_from_slice(&boxed(b"ispe", &full(0, &[0, 0, 0, 1, 0, 0, 0, 1])));
        if pixi {
            ipco.extend_from_slice(&boxed(b"pixi", &full(0, &[3, 8, 8, 8])));
        }
        ipco.extend_from_slice(&boxed(b"irot", &[0]));
        ipco.extend_from_slice(&boxed(b"pasp", &[0, 0, 0, 1, 0, 0, 0, 1]));
        for color in extra_colors {
            ipco.extend_from_slice(&boxed(b"colr", color));
        }
        let mut associations = (images.len() as u32).to_be_bytes().to_vec();
        for (id, _, _) in images {
            associations.extend_from_slice(&id.to_be_bytes());
            let colors = color_indexes
                .iter()
                .find(|(target, _)| target == id)
                .map_or(&[1u8][..], |(_, indexes)| indexes.as_slice());
            associations.push((if pixi { 5 } else { 4 }) + colors.len() as u8);
            associations.push(0x83);
            associations.extend_from_slice(colors);
            associations.extend_from_slice(if pixi {
                &[4, 0x82, 0x85, 6][..]
            } else {
                &[0x82, 0x84, 5][..]
            });
        }
        let mut iprp = boxed(b"ipco", &ipco);
        iprp.extend_from_slice(&boxed(b"ipma", &full(0, &associations)));
        meta.extend_from_slice(&boxed(b"iprp", &iprp));
        let mut references = Vec::new();
        for item in metadata {
            if let Some(target) = item.target {
                let mut reference = item.id.to_be_bytes().to_vec();
                reference.extend_from_slice(&1u16.to_be_bytes());
                reference.extend_from_slice(&target.to_be_bytes());
                references.extend_from_slice(&boxed(b"cdsc", &reference));
            }
        }
        if !references.is_empty() {
            meta.extend_from_slice(&boxed(b"iref", &full(0, &references)));
        }
        bytes.extend_from_slice(&boxed(b"meta", &full(0, &meta)));
        bytes
    }
    fn payload(bytes: &[u8]) -> Vec<u8> {
        let cancel = AtomicBool::new(false);
        let scan = inspect_directory(bytes, MAX_BYTES, &cancel).unwrap();
        let location = &scan.locations[0];
        scan.extents[location.extents.clone()]
            .iter()
            .flat_map(|range| bytes[range.clone()].iter().copied())
            .collect()
    }
    fn icc(kind: &[u8; 4], profile: &[u8]) -> Vec<u8> {
        let mut bytes = kind.to_vec();
        bytes.extend_from_slice(profile);
        bytes
    }
    fn sequence_position(bytes: &[u8]) -> usize {
        // 此7B真实样本为1×1、profile1、CICP1/13/6；位字段在分段前半中连续。
        bytes
            .windows(7)
            .position(|bytes| bytes == [0x38, 0, 6, 0x10, 0x10, 0xd0, 0x69])
            .unwrap()
    }

    #[test]
    fn image_platform_item_freezes_real_codecs_with_exact_lengths_and_minimum_brands() {
        for (source, limit, brand, ftyp) in [
            (
                &include_bytes!("../tests/data/image_platform/white_1x1.avif")[..],
                4,
                b"avif",
                28,
            ),
            (
                &include_bytes!("../tests/data/image_platform/hevc-confwin-420-8bit-left.heic")[..],
                250 * 128 * 4,
                b"heic",
                24,
            ),
        ] {
            let cancel = AtomicBool::new(false);
            let plan = inspect(source, limit, &cancel).unwrap();
            let frozen = plan.freeze_item(0, &cancel).unwrap();
            assert_eq!(frozen.bytes.len(), plan.frozen_bytes(0).unwrap());
            assert_eq!(
                u32::from_be_bytes(frozen.bytes[..4].try_into().unwrap()),
                ftyp
            );
            assert_eq!(&frozen.bytes[8..12], brand);
            assert_eq!(
                frozen.bytes[8..ftyp as usize]
                    .windows(4)
                    .any(|bytes| bytes == b"MA1A"),
                false
            );
            let checked = inspect(&frozen.bytes, limit, &cancel).unwrap();
            assert_eq!(checked.catalog().len(), 1);
            assert_eq!(checked.catalog()[0].item_id, 1);
            assert_eq!(checked.catalog()[0].coded, frozen.item.coded);
        }
    }

    #[test]
    fn image_platform_item_orders_primary_first_omits_hidden_and_isolates_logical_payloads() {
        let source = fixture(
            &[
                (19, false, b'A'),
                (7, false, b'B'),
                (3, true, b'C'),
                (50, false, b'D'),
            ],
            7,
            &[],
            true,
        );
        let cancel = AtomicBool::new(false);
        let plan = inspect(&source, 4, &cancel).unwrap();
        assert_eq!(plan.primary_index(), 0);
        assert_eq!(
            plan.catalog()
                .iter()
                .map(|item| item.item_id)
                .collect::<Vec<_>>(),
            [7, 19, 50]
        );
        assert_eq!(plan.items.len(), 4); // 隐藏项仍经过实际码流预检。
        for (index, marker) in [b'B', b'A', b'D'].into_iter().enumerate() {
            let frozen = plan.freeze_item(index, &cancel).unwrap();
            assert_eq!(frozen.item.item_id, [7, 19, 50][index]);
            assert_eq!(payload(&frozen.bytes), white_payload(marker));
            let directory = inspect_directory(&frozen.bytes, MAX_BYTES, &cancel).unwrap();
            assert_eq!(directory.items.len(), 1);
            assert_eq!(directory.items[0].id, 1);
            assert_eq!(directory.items[0].hidden, false);
            let kinds = directory
                .properties
                .iter()
                .map(|property| property.info().kind)
                .collect::<Vec<_>>();
            assert_eq!(kinds, [*b"ispe", *b"colr", *b"pixi", *b"av1C"]);
            let association = &directory.associations[0];
            assert_eq!(&association.indexes[..association.count], [1, 2, 3, 4]);
            assert_eq!(
                &association.essential[..association.count],
                [true, false, false, true]
            );
        }
        let original = inspect_directory(&source, MAX_BYTES, &cancel).unwrap();
        let hidden = original
            .locations
            .iter()
            .find(|location| location.id == 3)
            .unwrap();
        let mut broken_hidden = source.clone();
        broken_hidden[original.extents[hidden.extents.start].start] = 0xff;
        assert!(inspect(&broken_hidden, 4, &cancel).is_err());
    }

    #[test]
    fn image_platform_item_keeps_hidden_primary_fact_but_exposes_virtual_image() {
        let source = fixture(&[(19, false, b'A'), (3, true, b'C')], 3, &[], true);
        let cancel = AtomicBool::new(false);
        let plan = inspect(&source, 4, &cancel).unwrap();
        assert_eq!(plan.catalog()[0].item_id, 3);
        assert_eq!(plan.catalog()[0].hidden, true);
        let frozen = plan.freeze_item(0, &cancel).unwrap();
        let directory = inspect_directory(&frozen.bytes, MAX_BYTES, &cancel).unwrap();
        assert_eq!(directory.items[0].hidden, false);
        let original = inspect_directory(&source, MAX_BYTES, &cancel).unwrap();
        let hidden = original
            .locations
            .iter()
            .find(|location| location.id == 3)
            .unwrap();
        let first = original.extents[hidden.extents.start].start;
        let mut bad = source.clone();
        bad[first] = 0xff;
        assert!(inspect(&bad, 4, &cancel).is_err());
    }

    #[test]
    fn image_platform_item_binds_exif_by_cdsc_and_never_embeds_source_metadata() {
        let mut metadata = vec![
            exif(90, Some(19), 6),
            exif(91, Some(7), 3),
            exif(92, None, 9),
        ];
        metadata.push(Metadata {
            id: 93,
            kind: *b"mime",
            target: Some(7),
            payload: b"<xmp Orientation='8'/>".to_vec(),
        });
        let source = fixture(
            &[(19, false, b'A'), (7, false, b'B'), (50, false, b'D')],
            7,
            &metadata,
            true,
        );
        let cancel = AtomicBool::new(false);
        let plan = inspect(&source, 4, &cancel).unwrap();
        assert_eq!(
            plan.catalog()
                .iter()
                .map(|item| item.orientation.map(|value| value.to_exif()))
                .collect::<Vec<_>>(),
            [Some(3), Some(6), None]
        );
        for index in 0..plan.catalog().len() {
            let frozen = plan.freeze_item(index, &cancel).unwrap();
            let directory = inspect_directory(&frozen.bytes, MAX_BYTES, &cancel).unwrap();
            assert_eq!(directory.items.len(), 1);
            assert_eq!(directory.items[0].kind, ItemKind::Av1);
            assert!(directory.references.is_empty());
            assert_eq!(
                inspect(&frozen.bytes, 4, &cancel).unwrap().catalog()[0].orientation,
                None
            );
        }
        let duplicate = fixture(
            &[(19, false, b'A')],
            19,
            &[exif(90, Some(19), 6), exif(91, Some(19), 6)],
            true,
        );
        assert!(matches!(
            inspect(&duplicate, 4, &cancel),
            Err(super::super::PlatformError::Unsupported { .. })
        ));
        let malformed = fixture(&[(19, false, b'A')], 19, &[exif(90, Some(19), 9)], true);
        assert!(matches!(
            inspect(&malformed, 4, &cancel),
            Err(super::super::PlatformError::Invalid { .. })
        ));
    }

    #[test]
    fn image_platform_item_generates_missing_avif_pixi_and_rejects_contradictions() {
        let source = fixture(&[(19, false, b'A')], 19, &[], false);
        let cancel = AtomicBool::new(false);
        let plan = inspect(&source, 4, &cancel).unwrap();
        let frozen = plan.freeze_item(0, &cancel).unwrap();
        let checked = inspect(&frozen.bytes, 4, &cancel).unwrap();
        assert_eq!(checked.catalog()[0].coded.bits_per_channel, 8);
        let directory = inspect_directory(&frozen.bytes, MAX_BYTES, &cancel).unwrap();
        assert!(matches!(
            directory.properties.last(),
            Some(Property::Pixel(3, [8, 8, 8, 0], _))
        ));
        assert_eq!(directory.associations[0].essential[3], false);
        let original = fixture(&[(19, false, b'A')], 19, &[], true);
        let mut duplicate = original.clone();
        let pasp = duplicate
            .windows(4)
            .position(|bytes| bytes == b"pasp")
            .unwrap();
        duplicate[pasp..pasp + 4].copy_from_slice(b"pixi");
        duplicate[pasp + 4..pasp + 12].copy_from_slice(&[0, 0, 0, 0, 3, 8, 8, 8]);
        assert!(inspect_directory(&duplicate, MAX_BYTES, &cancel).is_ok());
        assert!(matches!(
            inspect(&duplicate, 4, &cancel),
            Err(super::super::PlatformError::Invalid { .. })
        ));
        let pixi = original
            .windows(4)
            .position(|bytes| bytes == b"pixi")
            .unwrap();
        let mut sixteen = original.clone();
        sixteen[pixi + 9..pixi + 12].copy_from_slice(&[16, 16, 16]);
        assert!(matches!(
            inspect(&sixteen, 4, &cancel),
            Err(super::super::PlatformError::Invalid { .. })
        ));
        let mut mono = original;
        mono[pixi + 8] = 1;
        mono.drain(pixi + 10..pixi + 12);
        // 更新pixi及各父box长度，保留一个结构有效但与实际三通道矛盾的声明。
        for kind in [b"meta", b"iprp", b"ipco", b"pixi"] {
            let position = mono.windows(4).position(|bytes| bytes == kind).unwrap() - 4;
            let size = u32::from_be_bytes(mono[position..position + 4].try_into().unwrap());
            mono[position..position + 4].copy_from_slice(&(size - 2).to_be_bytes());
        }
        assert!(inspect_directory(&mono, MAX_BYTES, &cancel).is_ok());
        assert!(matches!(
            inspect(&mono, 4, &cancel),
            Err(super::super::PlatformError::Invalid { .. })
        ));
    }

    #[test]
    fn image_platform_item_freeze_checks_index_and_cancel_before_allocating() {
        let source = fixture(&[(19, false, b'A')], 19, &[], true);
        let cancel = AtomicBool::new(false);
        let plan = inspect(&source, 4, &cancel).unwrap();
        assert!(plan.frozen_bytes(1).is_err());
        assert!(plan.freeze_item(1, &cancel).is_err());
        assert!(matches!(
            plan.freeze_item(usize::MAX, &AtomicBool::new(true)),
            Err(super::super::PlatformError::Cancelled)
        ));
    }

    #[test]
    fn image_platform_item_distinguishes_rgb_ycc_unknown_and_unmapped_coded_cicp() {
        let mut source = fixture(&[(19, false, b'A')], 19, &[], true);
        let ipma = source
            .windows(4)
            .position(|bytes| bytes == b"ipma")
            .unwrap();
        // 移除本图项的colr关联；全局仍存在的colr不能套到该图项。
        source[ipma + 16] = 0;
        let cancel = AtomicBool::new(false);
        assert!(inspect_directory(&source, MAX_BYTES, &cancel).is_ok());
        let plan = inspect(&source, 4, &cancel).unwrap();
        assert_eq!(plan.catalog()[0].color.container_cicp, None);
        assert_eq!(plan.catalog()[0].color.coded_color.model, SampleModel::Ycc);
        assert_eq!(plan.icc_profile(0).unwrap(), None);
        let sequence = sequence_position(&source);
        let mut rgb = source.clone();
        // matrix0走identity RGB分支，不再编码range位；尾部移动到bit54并补0。
        rgb[sequence + 6] = 0x02;
        assert!(matches!(
            inspect(&rgb, 4, &cancel),
            Err(super::super::PlatformError::Unsupported { .. })
        ));
        let mut unknown = source.clone();
        unknown[sequence + 6] = 0x29; // matrix2，仍显式编码range位。
        let unknown_plan = inspect(&unknown, 4, &cancel).unwrap();
        assert_eq!(
            unknown_plan.catalog()[0].color.coded_color.model,
            SampleModel::Unknown
        );
        assert_eq!(unknown_plan.catalog()[0].color.container_cicp, None);
        let mut pq = source;
        pq[sequence + 4] = 0x11;
        pq[sequence + 5] = 0; // transfer16，其它颜色字段不变。
        let pq_plan = inspect(&pq, 4, &cancel).unwrap();
        let color = pq_plan.catalog()[0].color;
        assert_eq!(color.container_cicp, None);
        assert_eq!(color.coded_color.cicp.unwrap().transfer, 16);
        assert_eq!(color.coded_color.model, SampleModel::Ycc);
    }

    #[test]
    fn image_platform_item_borrows_only_associated_icc_and_keeps_container_cicp_separate() {
        let srgb: &[u8] = include_bytes!("../assets/color/srgb.icc");
        let linear: &[u8] = include_bytes!("../tests/data/image_color/linear-rgb.icc");
        let colors = [icc(b"prof", srgb), icc(b"rICC", linear)];
        let source = fixture_colors(
            &[(19, false, b'A'), (7, false, b'B'), (50, false, b'C')],
            7,
            &[],
            true,
            &colors,
            &[(19, vec![1, 0x87]), (7, vec![8]), (50, vec![])],
        );
        let cancel = AtomicBool::new(false);
        let plan = inspect(&source, 4, &cancel).unwrap();
        assert_eq!(plan.icc_profile(0).unwrap(), Some(linear));
        assert_eq!(plan.icc_profile(1).unwrap(), Some(srgb));
        assert_eq!(plan.icc_profile(2).unwrap(), None);
        assert!(plan.icc_profile(3).is_err());
        for index in [0, 1] {
            let profile = plan.icc_profile(index).unwrap().unwrap();
            let pointer = profile.as_ptr() as usize;
            assert!(pointer >= source.as_ptr() as usize);
            assert!(pointer + profile.len() <= source.as_ptr() as usize + source.len());
        }
        assert_eq!(plan.catalog()[0].color.container_cicp, None);
        assert_eq!(plan.catalog()[0].color.coded_color.cicp.unwrap().matrix, 6);
        assert_eq!(
            plan.catalog()[1].color.container_cicp,
            Some(Cicp {
                primaries: 1,
                transfer: 13,
                matrix: 6,
                full_range: true
            })
        );
        let frozen = plan.freeze_item(1, &cancel).unwrap();
        let directory = inspect_directory(&frozen.bytes, MAX_BYTES, &cancel).unwrap();
        assert!(matches!(
            directory.properties[1],
            Property::Color(ColorProperty::Nclx(_), _)
        ));
        assert!(matches!(
            directory.properties[2],
            Property::Color(ColorProperty::Icc, _)
        ));
        assert_eq!(
            &directory.associations[0].essential[..5],
            [true, false, true, false, true]
        );
        let checked = inspect(&frozen.bytes, 4, &cancel).unwrap();
        assert_eq!(checked.icc_profile(0).unwrap(), Some(srgb));
        assert_eq!(
            checked.catalog()[0].color.container_cicp,
            plan.catalog()[1].color.container_cicp
        );
    }

    #[test]
    fn image_platform_item_rejects_duplicate_nclx_or_icc_without_rejecting_dual_types() {
        let srgb: &[u8] = include_bytes!("../assets/color/srgb.icc");
        let colors = [
            icc(b"prof", srgb),
            icc(b"rICC", srgb),
            b"nclx\0\x01\0\x0d\0\x06\x80".to_vec(),
        ];
        let cancel = AtomicBool::new(false);
        let both = fixture_colors(
            &[(19, false, b'A')],
            19,
            &[],
            true,
            &colors,
            &[(19, vec![1, 7])],
        );
        assert!(inspect(&both, 4, &cancel).is_ok());
        let nclx = fixture_colors(
            &[(19, false, b'A')],
            19,
            &[],
            true,
            &colors,
            &[(19, vec![1, 9])],
        );
        assert!(inspect_directory(&nclx, MAX_BYTES, &cancel).is_ok());
        assert!(matches!(
            inspect(&nclx, 4, &cancel),
            Err(super::super::PlatformError::Invalid { .. })
        ));
        let two_icc = fixture_colors(
            &[(19, false, b'A')],
            19,
            &[],
            true,
            &colors,
            &[(19, vec![7, 8])],
        );
        assert!(inspect_directory(&two_icc, MAX_BYTES, &cancel).is_ok());
        assert!(matches!(
            inspect(&two_icc, 4, &cancel),
            Err(super::super::PlatformError::Unsupported { .. })
        ));
        let mut rgb = fixture_colors(
            &[(19, false, b'A')],
            19,
            &[],
            true,
            &colors,
            &[(19, vec![7])],
        );
        let sequence = sequence_position(&rgb);
        rgb[sequence + 6] = 0x02;
        let rgb_plan = inspect(&rgb, 4, &cancel).unwrap();
        assert_eq!(
            rgb_plan.catalog()[0].color.coded_color.model,
            SampleModel::Rgb
        );
        assert_eq!(rgb_plan.catalog()[0].color.container_cicp, None);
        assert_eq!(rgb_plan.icc_profile(0).unwrap(), Some(srgb));
    }
}
