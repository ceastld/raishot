//! ICC字节结构摘要；通过边界检查不代表配置可用于颜色变换，也不解析标签内的字符串或LUT。
//! 长度、共享标签及对齐规则见https://www.color.org/specification/ICC.1-2022-05.pdf §7。

pub(crate) const MAX_PROFILE_BYTES: usize = 4 * 1024 * 1024;
const MIN_PROFILE_BYTES: usize = 132;
const MAX_TAGS: u32 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProfileSummary {
    pub declared_bytes: u32,
    pub version_major: u8,
    pub device_class: [u8; 4],
    pub color_space: [u8; 4],
    pub connection_space: [u8; 4],
    pub tag_count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TagSummary {
    pub signature: [u8; 4],
    pub data_type_signature: [u8; 4],
    pub offset: u32,
    /// 标签实际数据长度，不包括对齐用的补齐字节。
    pub declared_bytes: u32,
}

fn error(zh: &'static str, en: &'static str) -> String {
    crate::i18n::tr(zh, en).into()
}

fn be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[..4].try_into().unwrap())
}

fn tag_summary(bytes: &[u8], row: &[u8]) -> TagSummary {
    let offset = be32(&row[4..]);
    TagSummary {
        signature: row[..4].try_into().unwrap(),
        data_type_signature: bytes[offset as usize..offset as usize + 4]
            .try_into()
            .unwrap(),
        offset,
        declared_bytes: be32(&row[8..]),
    }
}

/// 只检查有界头、标签表和数据范围，保留未知签名；不检查各设备类别必需的变换标签。
pub(crate) fn inspect(bytes: &[u8]) -> Result<ProfileSummary, String> {
    if !(MIN_PROFILE_BYTES..=MAX_PROFILE_BYTES).contains(&bytes.len()) {
        return Err(error(
            "ICC配置长度须在132字节至4MiB之间",
            "ICC profile length must be between 132 bytes and 4 MiB",
        ));
    }
    let declared_bytes = be32(bytes);
    if declared_bytes as u64 != bytes.len() as u64 {
        return Err(error(
            "ICC声明长度与数据不一致",
            "ICC declared length does not match its data",
        ));
    }
    let version_major = bytes[8];
    if !matches!(version_major, 2 | 4) || &bytes[36..40] != b"acsp" {
        return Err(error(
            "ICC版本或文件签名不受支持",
            "Unsupported ICC version or file signature",
        ));
    }
    // v4末标签也须补齐至四字节；v2允许末标签以未补齐的数据长度结束。
    if version_major == 4 && bytes.len() % 4 != 0 {
        return Err(error(
            "ICC v4总长度未按四字节对齐",
            "ICC v4 total length is not four-byte aligned",
        ));
    }
    let tag_count = be32(&bytes[128..]);
    if tag_count > MAX_TAGS {
        return Err(error(
            "ICC标签数超过4096上限",
            "ICC tag count exceeds the 4096 limit",
        ));
    }
    let table_end = (tag_count as usize)
        .checked_mul(12)
        .and_then(|size| size.checked_add(MIN_PROFILE_BYTES))
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| error("ICC标签表被截断", "Truncated ICC tag table"))?;
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(tag_count as usize)
        .map_err(|_| error("ICC标签摘要分配失败", "ICC tag summary allocation failed"))?;
    for row in bytes[MIN_PROFILE_BYTES..table_end].chunks_exact(12) {
        let offset = be32(&row[4..]);
        let size = be32(&row[8..]);
        let end = offset
            .checked_add(size)
            .filter(|end| u64::from(*end) <= bytes.len() as u64)
            .ok_or_else(|| error("ICC标签超出配置范围", "ICC tag exceeds the profile range"))?;
        if offset % 4 != 0 || (offset as u64) < table_end as u64 || size < 8 {
            return Err(error(
                "ICC标签位置、对齐或类型头无效",
                "Invalid ICC tag offset, alignment or type header",
            ));
        }
        if bytes[offset as usize + 4..offset as usize + 8] != [0; 4] {
            return Err(error(
                "ICC标签类型头保留字节非零",
                "ICC tag type reserved bytes are nonzero",
            ));
        }
        if version_major == 4 {
            let padded_end = end
                .checked_add(3)
                .map(|end| end & !3)
                .filter(|end| u64::from(*end) <= bytes.len() as u64)
                .ok_or_else(|| error("ICC标签补齐字节被截断", "Truncated ICC tag padding"))?;
            if bytes[end as usize..padded_end as usize]
                .iter()
                .any(|byte| *byte != 0)
            {
                return Err(error("ICC标签补齐字节非零", "ICC tag padding is nonzero"));
            }
        }
        ranges.push(tag_summary(bytes, row));
    }
    ranges.sort_unstable_by_key(|tag| tag.signature);
    if ranges
        .windows(2)
        .any(|tags| tags[0].signature == tags[1].signature)
    {
        return Err(error("ICC标签签名重复", "Duplicate ICC tag signature"));
    }
    ranges.sort_unstable_by_key(|tag| (tag.offset, tag.declared_bytes));
    for tags in ranges.windows(2) {
        let (previous, next) = (tags[0], tags[1]);
        if previous.offset == next.offset && previous.declared_bytes == next.declared_bytes {
            continue;
        }
        if u64::from(next.offset) < u64::from(previous.offset) + u64::from(previous.declared_bytes)
        {
            return Err(error(
                "ICC标签数据部分重叠",
                "ICC tag data partially overlap",
            ));
        }
    }
    Ok(ProfileSummary {
        declared_bytes,
        version_major,
        device_class: bytes[12..16].try_into().unwrap(),
        color_space: bytes[16..20].try_into().unwrap(),
        connection_space: bytes[20..24].try_into().unwrap(),
        tag_count,
    })
}

/// 校验后借用原字节逐项返回，保留未知标签及类型签名，不复制标签内容。
pub(crate) fn tags(bytes: &[u8]) -> Result<impl ExactSizeIterator<Item = TagSummary> + '_, String> {
    let summary = inspect(bytes)?;
    let table_end = MIN_PROFILE_BYTES + summary.tag_count as usize * 12;
    Ok(bytes[MIN_PROFILE_BYTES..table_end]
        .chunks_exact(12)
        .map(move |row| tag_summary(bytes, row)))
}

/// 已知和私有签名均可显示；非ASCII或控制字节以十六进制显示，不透传控制符。
pub(crate) fn signature_label(signature: [u8; 4]) -> String {
    if signature.iter().all(|byte| (b' '..=b'~').contains(byte)) {
        signature.into_iter().map(char::from).collect()
    } else {
        format!(
            "0x{:02X}{:02X}{:02X}{:02X}",
            signature[0], signature[1], signature[2], signature[3]
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(version: u8, rows: &[([u8; 4], u32, u32)], length: usize) -> Vec<u8> {
        let mut bytes = vec![0; length];
        bytes[..4].copy_from_slice(&(length as u32).to_be_bytes());
        bytes[8] = version;
        bytes[12..16].copy_from_slice(b"mntr");
        bytes[16..20].copy_from_slice(b"RGB ");
        bytes[20..24].copy_from_slice(b"XYZ ");
        bytes[36..40].copy_from_slice(b"acsp");
        bytes[128..132].copy_from_slice(&(rows.len() as u32).to_be_bytes());
        for (index, (signature, offset, size)) in rows.iter().enumerate() {
            let start = MIN_PROFILE_BYTES + index * 12;
            bytes[start..start + 4].copy_from_slice(signature);
            bytes[start + 4..start + 8].copy_from_slice(&offset.to_be_bytes());
            bytes[start + 8..start + 12].copy_from_slice(&size.to_be_bytes());
            bytes[*offset as usize..*offset as usize + 4].copy_from_slice(b"priv");
        }
        bytes
    }

    #[test]
    fn image_color_profile_preserves_unknown_signatures_and_v2_unpadded_last_tag() {
        for version in [2, 4] {
            let length = if version == 2 { 153 } else { 156 };
            let mut bytes = profile(version, &[(*b"test", 144, 9)], length);
            bytes[12..16].copy_from_slice(&[0, b'X', 0xff, b' ']);
            bytes[16..20].copy_from_slice(b"priv");
            bytes[20..24].copy_from_slice(b"link");
            bytes[144..148].copy_from_slice(&[b'T', 0, 0xfe, b' ']);
            let summary = inspect(&bytes).unwrap();
            assert_eq!(summary.version_major, version);
            assert_eq!(summary.declared_bytes, length as u32);
            assert_eq!(summary.device_class, [0, b'X', 0xff, b' ']);
            assert_eq!(summary.color_space, *b"priv");
            assert_eq!(summary.connection_space, *b"link");
            let tag = tags(&bytes).unwrap().next().unwrap();
            assert_eq!(tag.signature, *b"test");
            assert_eq!(tag.data_type_signature, [b'T', 0, 0xfe, b' ']);
            assert_eq!(tag.declared_bytes, 9);
        }
        assert_eq!(signature_label(*b"RGB "), "RGB ");
        assert_eq!(signature_label([b'A', b'\n', 0x1b, 0xff]), "0x410A1BFF");
        assert!(signature_label([0, 0x7f, b'\r', b'\t'])
            .chars()
            .all(|ch| !ch.is_control()));
    }

    #[test]
    fn image_color_profile_shared_unsorted_tags_are_legal_but_partial_overlap_is_not() {
        let rows = [(*b"bTRC", 176, 8), (*b"rTRC", 168, 8), (*b"gTRC", 176, 8)];
        let bytes = profile(4, &rows, 184);
        assert_eq!(inspect(&bytes).unwrap().tag_count, 3);
        let tags: Vec<_> = tags(&bytes).unwrap().collect();
        assert_eq!(tags[0].offset, tags[2].offset);
        assert_eq!(tags[0].data_type_signature, *b"priv");
        let bytes = profile(4, &[(*b"one ", 156, 12), (*b"two ", 164, 8)], 172);
        assert!(inspect(&bytes).is_err());
        let bytes = profile(4, &[(*b"one ", 156, 12), (*b"two ", 156, 8)], 168);
        assert!(inspect(&bytes).is_err());
        let bytes = profile(4, &[(*b"same", 156, 8), (*b"same", 156, 8)], 164);
        assert!(inspect(&bytes).is_err());
    }

    #[test]
    fn image_color_profile_rejects_header_length_signature_and_version_bounds() {
        assert!(inspect(&[]).is_err());
        assert!(inspect(&vec![0; MIN_PROFILE_BYTES - 1]).is_err());
        assert!(inspect(&vec![0; MAX_PROFILE_BYTES + 1]).is_err());
        let mut bytes = profile(4, &[], MAX_PROFILE_BYTES);
        assert_eq!(
            inspect(&bytes).unwrap().declared_bytes as usize,
            MAX_PROFILE_BYTES
        );
        bytes[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(inspect(&bytes).is_err());
        let mut bytes = profile(4, &[], 132);
        bytes[36] = 0;
        assert!(inspect(&bytes).is_err());
        let mut bytes = profile(4, &[], 132);
        bytes[8] = 5;
        assert!(inspect(&bytes).is_err());
        assert!(inspect(&profile(4, &[], 133)).is_err());
    }

    #[test]
    fn image_color_profile_caps_tag_table_before_allocating_or_reading_entries() {
        let table_end = MIN_PROFILE_BYTES + MAX_TAGS as usize * 12;
        let rows: Vec<_> = (0..MAX_TAGS)
            .map(|index| (index.to_be_bytes(), table_end as u32, 8))
            .collect();
        let bytes = profile(4, &rows, table_end + 8);
        assert_eq!(inspect(&bytes).unwrap().tag_count, MAX_TAGS);
        let mut bytes = profile(4, &[], 132);
        bytes[128..132].copy_from_slice(&(MAX_TAGS + 1).to_be_bytes());
        assert!(inspect(&bytes).is_err());
        bytes[128..132].copy_from_slice(&1u32.to_be_bytes());
        assert!(inspect(&bytes).is_err());
    }

    #[test]
    fn image_color_profile_checks_tag_ranges_type_header_alignment_and_zero_padding() {
        let valid = profile(4, &[(*b"priv", 144, 9)], 156);
        for (offset, size) in [(145, 8), (128, 8), (u32::MAX, 8), (144, u32::MAX), (144, 7)] {
            let mut bytes = valid.clone();
            bytes[136..140].copy_from_slice(&offset.to_be_bytes());
            bytes[140..144].copy_from_slice(&size.to_be_bytes());
            assert!(inspect(&bytes).is_err());
        }
        let mut bytes = valid.clone();
        bytes[148] = 1;
        assert!(inspect(&bytes).is_err());
        let mut bytes = valid;
        bytes[155] = 1;
        assert!(inspect(&bytes).is_err());
        // 有界空隙不参与标签内容或颜色变换的解释。
        assert!(inspect(&profile(4, &[(*b"priv", 152, 8)], 160)).is_ok());
    }

    #[test]
    fn image_color_profile_reads_repository_srgb_profile_with_shared_tone_curves() {
        let bytes = include_bytes!("../assets/color/srgb.icc");
        let summary = inspect(bytes).unwrap();
        assert_eq!(summary.declared_bytes, 588);
        assert_eq!(summary.version_major, 4);
        assert_eq!(summary.device_class, *b"mntr");
        assert_eq!(summary.color_space, *b"RGB ");
        assert_eq!(summary.connection_space, *b"XYZ ");
        assert_eq!(summary.tag_count, 11);
        let tags: Vec<_> = tags(bytes).unwrap().collect();
        let curve = |signature| tags.iter().find(|tag| tag.signature == signature).unwrap();
        assert_eq!(curve(*b"rTRC").offset, curve(*b"gTRC").offset);
        assert_eq!(curve(*b"rTRC").offset, curve(*b"bTRC").offset);
        assert_eq!(curve(*b"rTRC").data_type_signature, *b"para");
    }
}
