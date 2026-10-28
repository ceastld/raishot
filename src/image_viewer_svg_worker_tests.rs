use super::snapshot_backpressure_tests::Worker;
use super::snapshot_tests::TempDir;
use super::*;
use crate::{image_color_management::ColorConversion, image_metadata::SourceFormat};

const SVG_SAMPLE: &[u8] = include_bytes!("../tests/data/image_decode/offline-vector.svg");

fn request(
    worker: &Worker,
    serial: u64,
    path: &Path,
    raster: Option<RasterRequest>,
    resident_bytes: u64,
) -> Completion {
    worker.loader.request(Request {
        serial,
        path: path.to_path_buf(),
        scan: false,
        page: None,
        raster,
        resident_bytes,
    });
    worker.wait_for(|mailbox| {
        mailbox
            .completed
            .as_ref()
            .is_some_and(|completion| completion.serial == serial)
    });
    worker.loader.take().unwrap()
}

#[test]
fn image_viewer_svg_worker_rasterizes_selected_size_without_mutating_previous_raw() {
    let temporary = TempDir::new();
    let path = temporary.path.join("source.svg");
    std::fs::write(&path, SVG_SAMPLE).unwrap();
    let worker = Worker::new(Mailbox::default());
    let first = request(&worker, 1, &path, None, THUMBNAIL_CACHE_BYTES as u64);
    let expected = first.source_file.unwrap();
    let original = first.result.unwrap();
    let original_raw = original.raw_page.as_ref().unwrap().clone();
    let original_pixels: [u8; 24] = original_raw.bgra.as_slice().try_into().unwrap();
    assert_eq!((original_raw.width, original_raw.height), (3, 2));
    let resident = presentation_resident_bytes(&original, std::iter::empty(), std::iter::empty());
    let completion = request(
        &worker,
        2,
        &path,
        Some(RasterRequest {
            dimensions: (6, 8),
            expected: expected.clone(),
        }),
        resident,
    );
    assert_eq!(completion.source_file.as_ref(), Some(&expected));
    let image = completion.result.unwrap();
    let raw = image.raw_page.as_ref().unwrap();
    assert_eq!((raw.width, raw.height), (6, 8));
    assert_eq!(raw.bgra.len(), 6 * 8 * 4);
    assert!(raw.bgra[..6 * 2 * 4].iter().all(|byte| *byte == 0));
    assert!(raw.bgra[6 * 6 * 4..].iter().all(|byte| *byte == 0));
    assert_eq!(&raw.bgra[6 * 2 * 4..][..4], &[80, 32, 208, 255]);
    assert_eq!(raw.bgra[(3 * 6 + 4) * 4 + 3], 128);
    assert_eq!(image.metadata.format, SourceFormat::Svg);
    assert_eq!(image.metadata.encoded_dimensions, None);
    assert_eq!(image.metadata.display_dimensions, (6, 8));
    assert_eq!(image.metadata.orientation, None);
    assert_eq!(image.metadata.encoded_bytes, Some(SVG_SAMPLE.len() as u64));
    assert_eq!(image.metadata.decoded_color, Some(image::ColorType::Rgba8));
    assert_eq!(
        image
            .metadata_source
            .as_ref()
            .unwrap()
            .facts()
            .color_conversion,
        ColorConversion::RenderedSrgb
    );
    assert!(!Arc::ptr_eq(&original_raw, raw));
    assert_eq!(original_raw.bgra.as_slice(), &original_pixels);
    assert_eq!(original.metadata.display_dimensions, (3, 2));
    worker.finish();
}

#[test]
fn image_viewer_svg_worker_failed_raster_preserves_identity_until_source_changes() {
    let temporary = TempDir::new();
    let path = temporary.path.join("source.svg");
    std::fs::write(&path, SVG_SAMPLE).unwrap();
    let worker = Worker::new(Mailbox::default());
    let first = request(&worker, 1, &path, None, THUMBNAIL_CACHE_BYTES as u64);
    let expected = first.source_file.unwrap();
    let original = first.result.unwrap();
    let original_raw = original.raw_page.as_ref().unwrap().clone();
    let original_pixels: [u8; 24] = original_raw.bgra.as_slice().try_into().unwrap();
    let completion = request(
        &worker,
        2,
        &path,
        Some(RasterRequest {
            dimensions: (6, 8),
            expected: expected.clone(),
        }),
        crate::image_decode::MAX_DECODE_BYTES,
    );
    assert!(completion.result.is_err());
    assert_eq!(completion.source_file.as_ref(), Some(&expected));
    assert_eq!(original_raw.bgra.as_slice(), &original_pixels);

    // 重栅格失败不能把同一身份的普通缩略缓存改成Failed。
    worker.loader.request_thumbnails(ThumbnailRequest {
        serial: 2,
        epoch: 1,
        resident_bytes: 0,
        cancel: Arc::new(AtomicBool::new(false)),
        paths: [(0, path.clone())].into(),
    });
    worker.wait_for(|mailbox| !mailbox.thumbnails.is_empty());
    let thumbnails = worker.loader.take_thumbnails();
    assert_eq!(thumbnails.len(), 1);
    assert!(matches!(&thumbnails[0].thumbnail, Thumbnail::Ready(_)));
    drop(thumbnails);

    // 只改测试临时副本；增加长度使完整身份确定变化，不依赖mtime分辨率。
    let mut changed = SVG_SAMPLE.to_vec();
    changed.extend_from_slice(b"\n<!-- changed temporary source -->");
    std::fs::write(&path, changed).unwrap();
    assert_ne!(source_file_snapshot(&path).as_ref(), Some(&expected));
    let resident = presentation_resident_bytes(&original, std::iter::empty(), std::iter::empty());
    let completion = request(
        &worker,
        3,
        &path,
        Some(RasterRequest {
            dimensions: (6, 8),
            expected,
        }),
        resident,
    );
    assert!(completion.result.is_err());
    assert!(completion.source_file.is_none());
    assert_eq!(original_raw.bgra.as_slice(), &original_pixels);
    assert_eq!(original.metadata.display_dimensions, (3, 2));
    worker.finish();
}
