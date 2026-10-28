use super::super::{
    presentation_resident_bytes, snapshot_tests::TempDir, source_snapshot, SnapshotRequest,
    SnapshotTarget,
};
use super::*;
use crate::image_decode::viewer::{decode_path, ViewerDecoded};
use std::{fs, io::Write};

const LIMIT: u64 = crate::image_decode::MAX_DECODE_BYTES;

/// 一行超65535边的真实PNG；只保留4KiB写入块，不创建完整native图片。
fn wide_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 200_000, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        {
            let mut stream = writer.stream_writer().unwrap();
            let mut row = [0u8; 4096];
            let mut start = 0u32;
            while start < 200_000 {
                let count = (200_000 - start).min(1024);
                for i in 0..count {
                    let band = ((start + i) / 50_000) as u8;
                    row[i as usize * 4..i as usize * 4 + 4].copy_from_slice(&[
                        3 + band * 60,
                        23 + band * 60,
                        43 + band * 60,
                        [0, 128, 255, 64][band as usize],
                    ]);
                }
                stream.write_all(&row[..count as usize * 4]).unwrap();
                start += count;
            }
            stream.finish().unwrap();
        }
        writer.finish().unwrap();
    }
    let exif = b"II\x2a\x00\x08\x00\x00\x00\x01\x00\x12\x01\x03\x00\x01\x00\x00\x00\x06\x00\x00\x00\x00\x00\x00\x00";
    let mut chunk = Vec::new();
    chunk.extend_from_slice(&(exif.len() as u32).to_be_bytes());
    chunk.extend_from_slice(b"eXIf");
    chunk.extend_from_slice(exif);
    let mut crc = !0u32;
    for byte in &chunk[4..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320u32 & 0u32.wrapping_sub(crc & 1));
        }
    }
    chunk.extend_from_slice(&(!crc).to_be_bytes());
    bytes.splice(33..33, chunk);
    bytes
}

fn decode_preview(path: &Path) -> (PreviewDecoded, u64, Arc<FrozenPreviewSource>) {
    match decode_path(path, LIMIT, 0, &AtomicBool::new(false)).unwrap() {
        ViewerDecoded::Preview {
            decoded,
            encoded_bytes,
            source,
        } => (decoded, encoded_bytes, source),
        ViewerDecoded::Full(_) => panic!("oversized PNG must retain explicit preview capability"),
    }
}

#[test]
fn image_viewer_preview_real_path_keeps_logical_source_and_denies_full_snapshots() {
    let temporary = TempDir::new();
    let path = temporary.path.join("wide-oriented.png");
    let bytes = wide_png();
    fs::write(&path, &bytes).unwrap();
    let (decoded, encoded_bytes, source) = decode_preview(&path);
    assert_eq!(encoded_bytes, bytes.len() as u64);
    assert_eq!(source.input(), bytes);
    assert_eq!(decoded.geometry.encoded_dimensions(), (200_000, 1));
    assert_eq!(
        decoded.geometry.normalized_source_dimensions(),
        (1, 200_000)
    );
    assert_eq!(decoded.geometry.sampled_encoded_dimensions(), (2048, 1));
    assert_eq!(decoded.geometry.raster_dimensions(), (1, 2048));
    let image = loaded_with_source(
        decoded,
        encoded_bytes,
        Some(Arc::clone(&source)),
        &AtomicBool::new(false),
        32 * 1024 * 1024,
    )
    .unwrap();
    assert!(!image.has_full_pixels());
    assert_eq!(image.logical_dimensions(), (1, 200_000));
    assert_eq!(image.logical_display_dimensions(), (1, 200_000));
    assert!(image.projection().is_none());
    assert_eq!(image.metadata.encoded_dimensions, Some((200_000, 1)));
    assert_eq!(image.metadata.display_dimensions, (1, 2048));
    assert_eq!(
        image.metadata.orientation,
        Some(image::metadata::Orientation::Rotate90)
    );
    assert_eq!(image.metadata.encoded_bytes, Some(encoded_bytes));
    assert_eq!(image.metadata.decoded_color, None);
    let details = image.metadata.details.unwrap();
    assert_eq!(details.source.bits_per_channel, Some(8));
    assert_eq!(details.profile, None);
    assert_eq!(details.color_conversion, ColorConversion::SignalUnmapped);
    assert!(image.warning.is_some());
    assert!(Arc::ptr_eq(image.preview_source.as_ref().unwrap(), &source));
    assert!(image.metadata_source.is_none());
    assert!(image.raw_page.is_none());
    assert!(image.raw_frame.is_none());

    assert_eq!(image.tiles.len(), 1);
    let (rect, tile) = &image.tiles[0];
    assert_eq!(*rect, Rect::full(1, 2048));
    let pixels = tile.as_bytes(0).unwrap();
    assert_eq!(pixels.len(), 2048 * 4);
    for band in 0..4u8 {
        let index = band as usize * 512 * 4;
        assert_eq!(
            &pixels[index..index + 4],
            &[
                43 + band * 60,
                23 + band * 60,
                3 + band * 60,
                [0, 128, 255, 64][band as usize],
            ]
        );
    }
    // 文件仍然存在也不能重新打开它，把采样结果升级成完整快照。
    for target in [
        SnapshotTarget::Editor,
        SnapshotTarget::Crop,
        SnapshotTarget::Ocr,
        SnapshotTarget::Pin { origin: (0, 0) },
        SnapshotTarget::Tools,
        SnapshotTarget::Clipboard {
            owner: 0,
            sequence: 0,
        },
    ] {
        let request = SnapshotRequest {
            serial: 1,
            path: path.clone(),
            frame: image.raw_frame.clone(),
            raw_page: image.raw_page.clone(),
            metadata_source: image.metadata_source.clone(),
            resident_bytes: presentation_resident_bytes(
                &image,
                std::iter::empty(),
                std::iter::empty(),
            ),
            target,
        };
        assert!(source_snapshot(&request).is_err());
    }
    assert!(path.is_file());
    // 总览透明像素保留隐藏RGB；替换路径不改变同一来源租约。
    fs::write(&path, b"replacement").unwrap();
    assert_eq!(source.input(), bytes);
}

#[test]
fn image_viewer_preview_presentation_budget_and_cancel_do_not_deliver_pixels() {
    let temporary = TempDir::new();
    let path = temporary.path.join("wide-oriented.png");
    fs::write(&path, wide_png()).unwrap();

    // 解码阶段成功后，宿主驻留增加也必须在呈现桥分配之前拒绝。
    let (decoded, encoded_bytes, source) = decode_preview(&path);
    assert!(loaded_with_source(
        decoded,
        encoded_bytes,
        Some(source),
        &AtomicBool::new(false),
        LIMIT
    )
    .is_err());
    let (decoded, encoded_bytes, source) = decode_preview(&path);
    let error = loaded_with_source(
        decoded,
        encoded_bytes,
        Some(source),
        &AtomicBool::new(true),
        0,
    )
    .err()
    .unwrap();
    assert!(error.contains("取消") || error.contains("canceled"));

    let (decoded, encoded_bytes, source) = decode_preview(&path);
    assert!(loaded_with_source(
        decoded,
        encoded_bytes + 1,
        Some(source),
        &AtomicBool::new(false),
        0
    )
    .is_err());

    // R只给纯总览的缩略采样留够空间；独占输入必须先真实销毁，才能重核此相位。
    let (decoded, encoded_bytes, source) = decode_preview(&path);
    let external = LIMIT
        - decoded.image.bgra.capacity() as u64
        - preview_warning(false).capacity() as u64
        - super::super::THUMBNAIL_WORKING_BYTES;
    let source_weak = Arc::downgrade(&source);
    let image = loaded_with_source(
        decoded,
        encoded_bytes,
        Some(source),
        &AtomicBool::new(false),
        external,
    )
    .unwrap();
    assert!(source_weak.upgrade().is_none());
    assert!(image.preview_source.is_none());
    assert_eq!(
        image.warning.as_deref(),
        Some(preview_warning(false).as_str())
    );
    assert_eq!(&image.tiles[0].1.as_bytes(0).unwrap()[..4], &[43, 23, 3, 0]);
    drop(image);

    let (decoded, encoded_bytes, source) = decode_preview(&path);
    let retained = Arc::clone(&source);
    assert!(loaded_with_source(
        decoded,
        encoded_bytes,
        Some(source),
        &AtomicBool::new(false),
        external,
    )
    .is_err());
    assert_eq!(Arc::strong_count(&retained), 1);
    drop(retained);

    // 兼容入口显式没有租约，仍只交付原始alpha总览。
    let (decoded, encoded_bytes, source) = decode_preview(&path);
    drop(source);
    let image = loaded(decoded, encoded_bytes, &AtomicBool::new(false), 0).unwrap();
    assert!(image.preview_source.is_none());
    assert_eq!(&image.tiles[0].1.as_bytes(0).unwrap()[..4], &[43, 23, 3, 0]);

    // path入口自身也保取消优先，失败不退回Full或创建预览呈现结果。
    let error = decode_path(
        &temporary.path.join("not-created.png"),
        LIMIT,
        LIMIT,
        &AtomicBool::new(true),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(error.contains("取消") || error.contains("canceled"));
    assert!(decode_path(&path, LIMIT, LIMIT, &AtomicBool::new(false)).is_err());
}

#[test]
fn image_viewer_preview_extreme_source_fit_and_zoom_keep_physical_anchor() {
    let geometry =
        crate::image_preview::PreviewGeometry::new((20_000_000, 1), (2048, 1), None).unwrap();
    let area = Rect {
        x: 37,
        y: 91,
        width: 800,
        height: 360,
    };
    let mut viewport = crate::image_preview::viewport::PreviewViewport::new(
        geometry.normalized_source_dimensions(),
    )
    .unwrap();
    viewport.fit(area).unwrap();
    assert!(viewport.zoom() > 0. && viewport.zoom() < 0.0001);
    let left = viewport.window_point((0., 0.)).unwrap();
    let right = viewport.window_point((20_000_000., 1.)).unwrap();
    assert!((left.0 - 37.).abs() < 1e-10);
    assert!((right.0 - 837.).abs() < 1e-10);
    assert!(left.1 >= 91. && right.1 <= 451.);
    assert!((viewport.zoom() * 20_000_000. - 800.).abs() < 1e-10);

    let fitted = viewport;
    let anchor = (357.25, 271.);
    let source = viewport.source_point(anchor).unwrap();
    let requested = viewport.zoom() * 1.2;
    viewport.set_zoom(requested, anchor).unwrap();
    assert_eq!(viewport.zoom(), requested);
    assert!(viewport.zoom() < 0.0001);
    let projected = viewport.window_point(source).unwrap();
    assert!((projected.0 - anchor.0).abs() < 1e-10);
    assert!((projected.1 - anchor.1).abs() < 1e-10);

    let mut maximum = fitted;
    maximum.set_zoom(f64::MAX, anchor).unwrap();
    assert_eq!(maximum.zoom(), 8.);
    let mut minimum = fitted;
    minimum.set_zoom(f64::MIN_POSITIVE, anchor).unwrap();
    assert!((minimum.zoom() * 20_000_000. - 1.).abs() < 1e-12);

    for requested in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1., 0.] {
        let mut unchanged = fitted;
        assert!(unchanged.set_zoom(requested, anchor).is_err());
        assert_eq!(unchanged, fitted);
    }
    for invalid_anchor in [(f64::NAN, 0.), (0., f64::INFINITY)] {
        let mut unchanged = fitted;
        assert!(unchanged.set_zoom(requested, invalid_anchor).is_err());
        assert_eq!(unchanged, fitted);
    }
    for invalid_offset in [(f64::INFINITY, 0.), (0., f64::NAN), (f64::MAX, 0.)] {
        let mut unchanged = fitted;
        assert!(unchanged.set_offset(invalid_offset).is_err());
        assert_eq!(unchanged, fitted);
    }
    for empty in [Rect { width: 0, ..area }, Rect { height: 0, ..area }] {
        let mut unchanged = fitted;
        assert!(unchanged.fit(empty).is_err());
        assert_eq!(unchanged, fitted);
    }
}
