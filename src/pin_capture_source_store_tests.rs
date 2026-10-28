use super::*;
use crate::pin_raster_document::capture_source::test_fixture::{Case, TestDir};
#[test]
fn internal_png_roundtrip_preserves_full_cpu_planes_and_standard_decoder_sees_only_raw_crop() {
    let case = Case::new();
    let dir = TestDir::new("archive");
    let path = dir.0.join("owned.png");
    let metadata = case.metadata();
    let raw = case.raw();
    save_png_pixels_with_context(&raw, &metadata, &case.pixels, &path).unwrap();
    let decoded = crate::export::read_image(&path).unwrap();
    assert_eq!((decoded.width, decoded.height), metadata.storage_size());
    assert_eq!(decoded.bgra, raw.bgra);
    assert_ne!(decoded.bgra, case.output(&case.editor).bgra);
    let restored = load_context(&path, &metadata, &decoded).unwrap();
    assert_eq!(restored.bounds, case.pixels.bounds);
    for (before, after) in case.pixels.planes.iter().zip(&restored.planes) {
        assert_eq!(before.rect, after.rect);
        assert_eq!(
            before.image.as_bytes(0).unwrap(),
            after.image.as_bytes(0).unwrap()
        );
    }
    assert_eq!(
        restored
            .source_pixels()
            .read(crate::geometry::Rect {
                x: 1,
                y: 2,
                width: 5,
                height: 6
            })
            .bgra,
        case.pixels
            .source_pixels()
            .read(crate::geometry::Rect {
                x: 1,
                y: 2,
                width: 5,
                height: 6
            })
            .bgra
    );
    assert!(source_reader(&path).is_ok());
    let plain = dir.0.join("user.png");
    crate::export::save_png_pixels(decoded.width, decoded.height, &decoded.bgra, &plain).unwrap();
    assert!(
        source_reader(&plain).is_err(),
        "用户普通PNG不带私有全桌面上下文"
    );
}
#[test]
fn archive_rejects_bad_crc_missing_context_extra_data_metadata_mismatch_and_wrong_visible_pixels() {
    let case = Case::new();
    let dir = TestDir::new("invalid");
    let path = dir.0.join("owned.png");
    let metadata = case.metadata();
    let raw = case.raw();
    save_png_pixels_with_context(&raw, &metadata, &case.pixels, &path).unwrap();
    let original = std::fs::read(&path).unwrap();
    let mut offset = 8usize;
    let mut data = 0usize;
    while offset < original.len() {
        let len = u32::from_be_bytes(original[offset..offset + 4].try_into().unwrap()) as usize;
        if original[offset + 4..offset + 8] == SOURCE_CHUNK {
            data = offset + 8;
            break;
        }
        offset += 12 + len;
    }
    assert!(data > 0);
    let mut bad = original.clone();
    bad[data] ^= 1;
    std::fs::write(&path, &bad).unwrap();
    assert!(load_context(&path, &metadata, &raw).is_err());
    let mut trailing = original.clone();
    trailing.push(0);
    std::fs::write(&path, &trailing).unwrap();
    assert!(load_context(&path, &metadata, &raw).is_err());
    std::fs::write(&path, &original).unwrap();
    let mut bad_meta = serde_json::to_value(&metadata).unwrap();
    bad_meta["left"] = serde_json::json!(-1920);
    let bad_meta = serde_json::from_value(bad_meta).unwrap();
    assert!(load_context(&path, &bad_meta, &raw).is_err());
    let mut wrong = case.raw();
    wrong.bgra[0] ^= 1;
    assert!(load_context(&path, &metadata, &wrong).is_err());
    let before = std::fs::read(&path).unwrap();
    assert!(save_png_pixels_with_context(&wrong, &metadata, &case.pixels, &path).is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "坏兼容帧必须在写文件前拒绝"
    );
    crate::export::save_png_pixels(raw.width, raw.height, &raw.bgra, &path).unwrap();
    assert!(load_context(&path, &metadata, &raw).is_err());
}
fn hostile_archive(
    path: &Path,
    metadata: &CaptureSourceMetadata,
    header: &[u8],
    plane: &[u8],
    extra: bool,
) {
    let mut archive = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    archive.start_file("source.json", options).unwrap();
    archive.write_all(header).unwrap();
    archive.start_file("plane-00.bgra", options).unwrap();
    archive.write_all(plane).unwrap();
    if extra {
        archive.start_file("unexpected.bgra", options).unwrap();
        archive.write_all(&[0]).unwrap();
    }
    let bytes = archive.finish().unwrap().into_inner();
    wrap_archive_png(path, &bytes);
    assert!(metadata.validate().is_ok());
}
fn wrap_archive_png(path: &Path, bytes: &[u8]) {
    let raw = Case::new().raw();
    crate::export::save_png(raw, path).unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let n = file.metadata().unwrap().len();
    file.set_len(n - 12).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    let table = crc_table();
    for chunk in bytes.chunks(CHUNK_BYTES) {
        file.write_all(&(chunk.len() as u32).to_be_bytes()).unwrap();
        file.write_all(&SOURCE_CHUNK).unwrap();
        file.write_all(chunk).unwrap();
        let crc = crc_update(crc_update(u32::MAX, &SOURCE_CHUNK, &table), chunk, &table) ^ u32::MAX;
        file.write_all(&crc.to_be_bytes()).unwrap();
    }
    file.write_all(&IEND).unwrap();
    file.sync_all().unwrap();
}
#[test]
fn archive_bounded_decompression_rejects_wrong_plane_length_oversized_header_and_extra_plane() {
    let case = Case::new();
    let dir = TestDir::new("bounded");
    let path = dir.0.join("owned.png");
    let metadata = case.metadata();
    let header = serde_json::to_vec(&metadata).unwrap();
    let plane = case.pixels.planes[0].image.as_bytes(0).unwrap();
    let raw = case.raw();
    for (header, plane, extra) in [
        (header.clone(), plane[..plane.len() - 1].to_vec(), false),
        (
            vec![b' '; MAX_METADATA_BYTES as usize + 1],
            plane.to_vec(),
            false,
        ),
        (header, plane.to_vec(), true),
    ] {
        hostile_archive(&path, &metadata, &header, &plane, extra);
        let bytes = std::fs::read(&path).unwrap();
        assert!(load_context(&path, &metadata, &raw).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn archive_directory_budget_is_checked_before_zip_constructor_count_offset_or_multidisk_allocation()
{
    let case = Case::new();
    let dir = TestDir::new("directory");
    let path = dir.0.join("owned.png");
    let metadata = case.metadata();
    let raw = case.raw();
    save_png_pixels_with_context(&raw, &metadata, &case.pixels, &path).unwrap();
    let mut reader = source_reader(&path).unwrap();
    preflight_zip32(&mut reader, &metadata).unwrap();
    let mut archive = Vec::new();
    reader.read_to_end(&mut archive).unwrap();
    drop(reader);
    let end = archive.len() - 22;
    assert_eq!(le32(&archive, end), 0x06054b50);
    let central = le32(&archive, end + 16) as usize;
    let mut ntfs = archive.clone();
    let at = central + 46 + usize::from(le16(&archive, central + 28));
    ntfs.splice(at..at, [0x0a, 0, 0, 0]);
    ntfs[central + 30..central + 32].copy_from_slice(&4u16.to_le_bytes());
    let new_end = ntfs.len() - 22;
    let directory_size = le32(&archive, end + 12) + 4;
    ntfs[new_end + 12..new_end + 16].copy_from_slice(&directory_size.to_le_bytes());
    let mutate = |at: usize, value: &[u8]| {
        let mut bytes = archive.clone();
        bytes[at..at + value.len()].copy_from_slice(value);
        bytes
    };
    for bytes in [
        mutate(end + 8, &u16::MAX.to_le_bytes()),
        mutate(end + 10, &u16::MAX.to_le_bytes()),
        mutate(end + 16, &u32::MAX.to_le_bytes()),
        mutate(end + 12, &(65 * 1024u32).to_le_bytes()),
        mutate(end + 4, &1u16.to_le_bytes()),
        mutate(central + 8, &1u16.to_le_bytes()),
        ntfs,
    ] {
        wrap_archive_png(&path, &bytes);
        let before = std::fs::read(&path).unwrap();
        let mut reader = source_reader(&path).unwrap();
        assert!(
            preflight_zip32(&mut reader, &metadata).is_err(),
            "声明项数/范围应在ZipArchive预分配之前拒绝"
        );
        assert!(load_context(&path, &metadata, &raw).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
