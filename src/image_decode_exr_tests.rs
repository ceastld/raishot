use super::*;
use exr::prelude::*;
use image::ImageDecoder;
use std::io::Cursor;

fn encoded(alpha: bool) -> Vec<u8> {
    let mut descriptions = vec![
        ("R", vec![0.25, -1.0, 1.0, 2.0]),
        ("G", vec![0.5, 0.0, 0.75, 0.125]),
        ("B", vec![0.75, 1.0, 0.0, 0.5]),
    ];
    if alpha {
        descriptions.push(("A", vec![0.0, 0.5, 1.0, 0.25]));
    }
    let channels = AnyChannels::sort(
        descriptions
            .into_iter()
            .map(|(name, samples)| AnyChannel::new(name, FlatSamples::F32(samples)))
            .collect(),
    );
    let layer = Layer::new(
        (2, 2),
        LayerAttributes::named("color").with_position(Vec2(-1, 5)),
        Encoding {
            compression: Compression::ZIP16,
            blocks: Blocks::ScanLines,
            line_order: LineOrder::Increasing,
        },
        channels,
    );
    let image = exr::image::Image::new(
        ImageAttributes::new(IntegerBounds::new((-2, 4), (3, 3))),
        layer,
    );
    let mut output = Cursor::new(Vec::new());
    image
        .write()
        .non_parallel()
        .to_buffered(&mut output)
        .unwrap();
    output.into_inner()
}

fn legacy_native(bytes: &[u8]) -> Vec<u8> {
    let decoder = image::codecs::openexr::OpenExrDecoder::new(Cursor::new(bytes)).unwrap();
    assert_eq!(decoder.dimensions(), (3, 3));
    let mut output = vec![0xa5; decoder.total_bytes() as usize];
    decoder.read_image(&mut output).unwrap();
    output
}

fn legacy_bgra(bytes: &[u8]) -> Vec<u8> {
    let decoder = image::codecs::openexr::OpenExrDecoder::new(Cursor::new(bytes)).unwrap();
    let mut rgba = image::DynamicImage::from_decoder(decoder)
        .unwrap()
        .into_rgba8()
        .into_raw();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    rgba
}

#[test]
fn image_decode_exr_workspace_native_and_foreground_keep_float_display_and_metadata() {
    let cancel = AtomicBool::new(false);
    let external = 32 * 1024 * 1024;
    for alpha in [false, true] {
        let bytes = encoded(alpha);
        let resident = external + bytes.capacity() as u64;
        let prepared = inspect(&bytes, resident, &cancel).unwrap();
        assert_eq!(prepared.workspace.dimensions, (3, 3));
        assert_eq!(
            prepared.workspace.native_bytes,
            9 * if alpha { 16 } else { 12 }
        );
        assert_eq!(prepared.selection.part_index, 0);
        assert_eq!(prepared.selection.has_alpha, alpha);
        assert_eq!(
            prepared.selection.data_window,
            exr_adapter::Window {
                x: -1,
                y: 5,
                width: 2,
                height: 2
            }
        );
        assert_eq!(
            prepared.selection.display_window,
            exr_adapter::Window {
                x: -2,
                y: 4,
                width: 3,
                height: 3
            }
        );

        // 真正的串行adapter写caller native，与旧公开decoder逐bit对照。
        let decoder = exr_adapter::Decoder::new(&bytes, &cancel, |source, flag| {
            Ok(inspect(source, resident, flag)?.selection)
        })
        .unwrap();
        let mut native = vec![0xa5; decoder.total_bytes() as usize];
        decoder.read_image(&mut native).unwrap();
        assert_eq!(native, legacy_native(&bytes));

        let ordinary = super::super::decode_bytes(&bytes).unwrap();
        // 必须经过统一生产Foreground分派；不以单独调用inspect代替接线回归。
        let foreground = super::super::decode_bytes_inner_with_budget(
            &bytes,
            bytes.capacity() as u64,
            super::super::MAX_DECODE_BYTES,
            &cancel,
            super::super::DecodeContent::File,
            super::super::DecodeBudget::Foreground(external),
        )
        .unwrap();
        let expected = legacy_bgra(&bytes);
        assert_eq!(ordinary.image.bgra, expected);
        assert_eq!(foreground.image.bgra, expected);
        assert_eq!((foreground.image.width, foreground.image.height), (3, 3));
        assert_eq!(foreground.metadata, ordinary.metadata);
        assert_eq!(foreground.warning, ordinary.warning);
        assert_eq!(
            foreground
                .metadata_source
                .as_ref()
                .map(|source| source.facts()),
            ordinary
                .metadata_source
                .as_ref()
                .map(|source| source.facts())
        );
        assert!(foreground.metadata.format == image::ImageFormat::OpenExr);
        assert_eq!(foreground.metadata.encoded_dimensions, Some((3, 3)));
        assert_eq!(foreground.metadata.display_dimensions, (3, 3));
        assert_eq!(foreground.metadata.encoded_bytes, Some(bytes.len() as u64));
        assert_eq!(
            foreground.metadata.decoded_color,
            Some(if alpha {
                image::ColorType::Rgba32F
            } else {
                image::ColorType::Rgb32F
            })
        );
        let alpha_at = |x: usize, y: usize| foreground.image.bgra[(y * 3 + x) * 4 + 3];
        if alpha {
            assert_eq!(
                [
                    alpha_at(1, 1),
                    alpha_at(2, 1),
                    alpha_at(1, 2),
                    alpha_at(2, 2)
                ],
                [0, 128, 255, 64]
            );
            assert_eq!(&foreground.image.bgra[..4], &[0, 0, 0, 0]);
            assert!(foreground.image.bgra[(3 + 1) * 4..(3 + 1) * 4 + 3]
                .iter()
                .any(|value| *value != 0));
        } else {
            assert!(foreground
                .image
                .bgra
                .chunks_exact(4)
                .all(|pixel| pixel[3] == 255));
        }
    }
}

#[test]
fn image_decode_exr_workspace_exact_stage_boundaries_and_cancellation() {
    // Box<[u8]>的来源reservation就是len，不把未知Vec slack冒充可从slice观察的事实。
    let source = encoded(true).into_boxed_slice();
    let cancel = AtomicBool::new(false);
    let prepared = inspect(&source, source.len() as u64, &cancel).unwrap();
    let workspace = prepared.workspace;
    let stage = workspace.constructor_bytes.max(
        workspace
            .native_bytes
            .checked_add(workspace.decode_extra_bytes)
            .unwrap(),
    );
    let exact = super::super::MAX_DECODE_BYTES.checked_sub(stage).unwrap();
    assert!(exact >= source.len() as u64);
    let boundary = inspect(&source, exact, &cancel).unwrap();
    assert_eq!(boundary.workspace.dimensions, workspace.dimensions);
    assert_eq!(boundary.workspace.native_bytes, workspace.native_bytes);
    assert_eq!(
        boundary.workspace.constructor_bytes,
        workspace.constructor_bytes
    );
    assert_eq!(
        boundary.workspace.decode_extra_bytes,
        workspace.decode_extra_bytes
    );
    assert!(inspect(&source, exact + 1, &cancel).is_err());
    assert!(inspect(&source, source.len() as u64 - 1, &cancel).is_err());
    assert!(inspect(&source, u64::MAX, &cancel).is_err());
    assert!(inspect(b"not EXR", 7, &cancel).is_err());
    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    for (bytes, resident) in [
        (&source[..], u64::MAX),
        (&b"not EXR"[..], u64::MAX),
        (&[][..], 0),
    ] {
        let error = inspect(bytes, resident, &cancel).err().unwrap().to_string();
        assert_eq!(error, tr!("图片读取已取消", "Image reading canceled"));
    }
}
