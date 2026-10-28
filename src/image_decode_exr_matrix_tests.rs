//! 由exr_pipeline直接挂载的真实压缩矩阵；样本均由公开串行writer现场产生。
use super::*;
use exr::prelude::*;
use image::ImageDecoder;
use std::io::Cursor;

const DIMENSIONS: (u32, u32) = (64, 64);

fn samples(x: usize, y: usize) -> [f32; 4] {
    // 16像素色块让DCT的8×8块、B44的4×4块都包含大量重复值。
    // RGB保留非整half的小数，让PXR24的F32分支也得到实际输入。
    [
        ((x / 16) + 1) as f32 * 0.125 + 0.000_031,
        ((y / 16) + 1) as f32 * 0.125 + 0.000_061,
        if (x / 16 + y / 16) % 2 == 0 {
            0.750_017
        } else {
            0.250_017
        },
        [0.0, 0.25, 0.5, 1.0][x / 16],
    ]
}

fn encoded(compression: Compression, full_precision: bool) -> Vec<u8> {
    let channels = AnyChannels::sort(
        ["R", "G", "B", "A"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| {
                let values: Vec<f32> = (0..DIMENSIONS.1 as usize)
                    .flat_map(|y| (0..DIMENSIONS.0 as usize).map(move |x| samples(x, y)[index]))
                    .collect();
                let data = if full_precision {
                    FlatSamples::F32(values)
                } else {
                    FlatSamples::F16(values.into_iter().map(f16::from_f32).collect())
                };
                AnyChannel::new(name, data)
            })
            .collect(),
    );
    let image = exr::image::Image::from_encoded_channels(
        (DIMENSIONS.0 as usize, DIMENSIONS.1 as usize),
        Encoding {
            compression,
            blocks: Blocks::ScanLines,
            line_order: LineOrder::Increasing,
        },
        channels,
    );
    let mut output = Cursor::new(Vec::new());
    image
        .write()
        .non_parallel()
        .to_buffered(&mut output)
        .unwrap_or_else(|error| panic!("writer {compression:?}: {error}"));
    output.into_inner()
}

fn legacy_native(bytes: &[u8]) -> Vec<u8> {
    let decoder = image::codecs::openexr::OpenExrDecoder::new(Cursor::new(bytes)).unwrap();
    assert_eq!(decoder.dimensions(), DIMENSIONS);
    assert_eq!(decoder.color_type(), image::ColorType::Rgba32F);
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
fn image_decode_exr_workspace_real_compression_matrix() {
    let cancel = AtomicBool::new(false);
    let external = 32 * 1024 * 1024;
    let cases = [
        (0, Compression::Uncompressed, true),
        (1, Compression::RLE, true),
        (2, Compression::ZIP1, true),
        (3, Compression::ZIP16, true),
        (4, Compression::PIZ, false),
        (5, Compression::PXR24, true),
        (6, Compression::B44, false),
        (7, Compression::B44A, false),
        (8, Compression::DWAA(None), false),
        (9, Compression::DWAB(None), false),
    ];
    for (code, compression, full_precision) in cases {
        let bytes = encoded(compression, full_precision);
        let resident = external + bytes.capacity() as u64;
        let directory = exr_budget::inspect(&bytes, resident, &cancel)
            .unwrap_or_else(|error| panic!("directory {compression:?}: {error}"));
        assert_eq!(directory.selection.compression, code, "{compression:?}");
        assert!(directory.selection.has_alpha, "{compression:?}");
        assert_eq!(directory.selection.part_index, 0);
        assert_eq!(directory.selection.channel_count, 4);
        assert!(directory
            .channels()
            .iter()
            .all(|channel| channel.sample_type == if full_precision { 2 } else { 1 }));
        let mut selected = 0;
        let mut compressed = 0;
        directory
            .for_each_selected_chunk(&cancel, |chunk| {
                assert_eq!(chunk.compression, code, "{compression:?}");
                assert_eq!(chunk.dimensions.0, DIMENSIONS.0);
                let expected_raw = u64::from(chunk.dimensions.0)
                    * u64::from(chunk.dimensions.1)
                    * if full_precision { 16 } else { 8 };
                assert_eq!(chunk.raw_block_bytes, expected_raw, "{compression:?}");
                assert!(!chunk.payload.is_empty(), "{compression:?}");
                selected += 1;
                if chunk.payload.len() as u64 != chunk.raw_block_bytes {
                    assert!(
                        (chunk.payload.len() as u64) < chunk.raw_block_bytes,
                        "writer must keep compression only when smaller: {compression:?}"
                    );
                    compressed += 1;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(selected, directory.selection.selected_chunk_count);
        assert!(selected > 0, "{compression:?}");
        if code == 0 {
            assert_eq!(compressed, 0);
        } else {
            // 不允许raw fallback让某种压缩器的真实解块回归假通过。
            assert!(
                compressed > 0,
                "no actual compressed block for {compression:?}"
            );
        }

        let prepared = inspect(&bytes, resident, &cancel)
            .unwrap_or_else(|error| panic!("workspace {compression:?}: {error}"));
        assert_eq!(prepared.workspace.dimensions, DIMENSIONS);
        assert_eq!(
            prepared.workspace.native_bytes,
            u64::from(DIMENSIONS.0) * u64::from(DIMENSIONS.1) * 16
        );
        assert!(prepared.workspace.constructor_bytes > 0);
        assert!(prepared.workspace.decode_extra_bytes > 0);

        let decoder = exr_adapter::Decoder::new(&bytes, &cancel, |source, flag| {
            Ok(inspect(source, resident, flag)?.selection)
        })
        .unwrap_or_else(|error| panic!("adapter {compression:?}: {error}"));
        let mut native = vec![0xa5; decoder.total_bytes() as usize];
        decoder
            .read_image(&mut native)
            .unwrap_or_else(|error| panic!("native {compression:?}: {error}"));
        // 有损family也只与同份实际码流的旧后端结果对照，不要求恢复源float位。
        assert_eq!(native, legacy_native(&bytes), "native {compression:?}");
        drop(native);

        let expected = legacy_bgra(&bytes);
        let ordinary = super::super::decode_bytes(&bytes)
            .unwrap_or_else(|error| panic!("ordinary {compression:?}: {error}"));
        let foreground = super::super::decode_bytes_inner_with_budget(
            &bytes,
            bytes.capacity() as u64,
            super::super::MAX_DECODE_BYTES,
            &cancel,
            super::super::DecodeContent::File,
            super::super::DecodeBudget::Foreground(external),
        )
        .unwrap_or_else(|error| panic!("foreground {compression:?}: {error}"));
        assert_eq!(ordinary.image.bgra, expected, "ordinary {compression:?}");
        assert_eq!(
            foreground.image.bgra, expected,
            "foreground {compression:?}"
        );
        assert_eq!(
            (foreground.image.width, foreground.image.height),
            DIMENSIONS,
            "{compression:?}"
        );
        // 全图比较包括透明及部分透明像素；B44/A的线性alpha也会有损。
        assert!(
            expected.chunks_exact(4).any(|pixel| pixel[3] == 0),
            "missing transparent alpha: {compression:?}"
        );
        if matches!(code, 6 | 7) {
            // A通道的flat block仍经过EXP/LOG表。四个half输入0/.25/.5/1
            // 回读bits为0000/340f/37ff/3bfb；最后者为0.99755859375。
            // 用这些独立字面参考验证量化结果，不把alpha=255误当作有损合同。
            let bands = [0usize, 16, 32, 48].map(|x| expected[x * 4 + 3]);
            assert_eq!(bands, [0, 65, 127, 254], "alpha bands: {compression:?}");
        } else {
            assert!(
                expected.chunks_exact(4).any(|pixel| pixel[3] == 255),
                "missing opaque alpha: {compression:?}"
            );
        }
        assert!(
            expected
                .chunks_exact(4)
                .any(|pixel| (1..=254).contains(&pixel[3])),
            "missing partial alpha: {compression:?}"
        );
        assert_eq!(foreground.metadata, ordinary.metadata, "{compression:?}");
        assert_eq!(foreground.warning, ordinary.warning, "{compression:?}");
        assert_eq!(
            foreground
                .metadata_source
                .as_ref()
                .map(|source| source.facts()),
            ordinary
                .metadata_source
                .as_ref()
                .map(|source| source.facts()),
            "{compression:?}"
        );
        assert_eq!(foreground.metadata.format, image::ImageFormat::OpenExr);
        assert_eq!(foreground.metadata.encoded_dimensions, Some(DIMENSIONS));
        assert_eq!(foreground.metadata.display_dimensions, DIMENSIONS);
        assert_eq!(foreground.metadata.encoded_bytes, Some(bytes.len() as u64));
        assert_eq!(
            foreground.metadata.decoded_color,
            Some(image::ColorType::Rgba32F)
        );
    }
}
