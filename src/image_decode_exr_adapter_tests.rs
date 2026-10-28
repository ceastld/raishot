use super::*;
use std::cell::Cell;

fn window(x: i32, y: i32, width: u32, height: u32) -> Window {
    Window {
        x,
        y,
        width,
        height,
    }
}

fn selection(display: Window, data: Window, has_alpha: bool, part_index: usize) -> Selection {
    Selection {
        display_window: display,
        data_window: data,
        has_alpha,
        part_index,
    }
}

fn encode<'a, L>(image: &'a exr::image::Image<L>) -> Vec<u8>
where
    L: exr::image::write::layers::WritableLayers<'a>,
{
    let mut output = Cursor::new(Vec::new());
    image
        .write()
        .non_parallel()
        .to_buffered(&mut output)
        .unwrap();
    output.into_inner()
}

fn flat_channels(names: &[&str], samples: &[Vec<f32>]) -> AnyChannels<FlatSamples> {
    AnyChannels::sort(
        names
            .iter()
            .zip(samples)
            .map(|(name, samples)| AnyChannel::new(*name, FlatSamples::F32(samples.clone())))
            .collect(),
    )
}

fn flat(
    display: Window,
    data: Window,
    names: &[&str],
    samples: &[Vec<f32>],
    compression: Compression,
    blocks: Blocks,
) -> Vec<u8> {
    encode(&exr::image::Image::new(
        ImageAttributes::new(IntegerBounds::new(
            (display.x, display.y),
            (display.width as usize, display.height as usize),
        )),
        Layer::new(
            (data.width as usize, data.height as usize),
            LayerAttributes::named("pixels").with_position(Vec2(data.x, data.y)),
            Encoding {
                compression,
                blocks,
                line_order: LineOrder::Increasing,
            },
            flat_channels(names, samples),
        ),
    ))
}

fn compare(bytes: &[u8], selection: Selection) -> Vec<u8> {
    let cancel = AtomicBool::new(false);
    let old = image::codecs::openexr::OpenExrDecoder::new(Cursor::new(bytes)).unwrap();
    let new = Decoder::new(bytes, &cancel, |_, _| Ok(selection)).unwrap();
    assert_eq!(new.dimensions(), old.dimensions());
    assert_eq!(new.color_type(), old.color_type());
    assert_eq!(new.original_color_type(), old.original_color_type());
    let mut expected = vec![0xa5; old.total_bytes() as usize];
    let mut actual = vec![0xa5; new.total_bytes() as usize];
    old.read_image(&mut expected).unwrap();
    new.read_image(&mut actual).unwrap();
    assert_eq!(actual, expected);

    let old = image::DynamicImage::from_decoder(
        image::codecs::openexr::OpenExrDecoder::new(Cursor::new(bytes)).unwrap(),
    )
    .unwrap();
    let new = image::DynamicImage::from_decoder(
        Decoder::new(bytes, &cancel, |_, _| Ok(selection)).unwrap(),
    )
    .unwrap();
    assert_eq!(new.into_rgba8().into_raw(), old.into_rgba8().into_raw());
    actual
}

#[test]
fn image_decode_exr_adapter_approval_precedes_backend_and_cancellation_wins() {
    let cancel = AtomicBool::new(false);
    let called = Cell::new(false);
    let error = Decoder::new(b"not an EXR", &cancel, |_, _| {
        called.set(true);
        Err("approval denied".into())
    })
    .err()
    .unwrap();
    assert!(called.get());
    assert_eq!(error.to_string(), "approval denied");
    let error = Decoder::new(b"not an EXR", &cancel, |_, flag| {
        flag.store(true, Ordering::Relaxed);
        Err("approval denied".into())
    })
    .err()
    .unwrap();
    assert_eq!(
        error.to_string(),
        tr!("图片读取已取消", "Image reading canceled")
    );
    let called = Cell::new(false);
    assert!(Decoder::new(b"", &cancel, |_, _| {
        called.set(true);
        unreachable!()
    })
    .is_err());
    assert!(!called.get());
}

#[test]
fn image_decode_exr_adapter_float_bits_alpha_and_existing_quantization() {
    let display = window(0, 0, 3, 1);
    let samples = [
        vec![0.0, -0.0, f32::from_bits(0x7fc0_1234)],
        vec![-1.0, 0.5, f32::INFINITY],
        vec![2.0, f32::NEG_INFINITY, 1.0 / 255.0],
        vec![0.0, 0.5, 1.0],
    ];
    let bytes = flat(
        display,
        display,
        &["R", "G", "B", "A"],
        &samples,
        Compression::Uncompressed,
        Blocks::ScanLines,
    );
    let output = compare(&bytes, selection(display, display, true, 0));
    let expected: Vec<_> = (0..3)
        .flat_map(|pixel| {
            samples
                .iter()
                .flat_map(move |channel| channel[pixel].to_ne_bytes())
        })
        .collect();
    assert_eq!(output, expected);
    // 不因A=0丢失隐藏RGB，也不对浮点源加传递函数或颜色变换。
    assert_eq!(&output[4..8], &(-1.0f32).to_ne_bytes());
}

#[test]
fn image_decode_exr_adapter_display_window_crops_and_keeps_blank_pixels() {
    let display = window(-2, 3, 3, 2);
    let samples = [vec![1.0; 4], vec![0.5; 4], vec![0.25; 4], vec![0.75; 4]];
    for (data, alpha) in [
        (window(-1, 4, 2, 2), true),
        (window(-3, 2, 2, 2), true),
        (window(20, 30, 2, 2), false),
    ] {
        let channels = if alpha { 4 } else { 3 };
        let bytes = flat(
            display,
            data,
            &["R", "G", "B", "A"][..channels],
            &samples[..channels],
            Compression::ZIP1,
            Blocks::ScanLines,
        );
        let actual = compare(&bytes, selection(display, data, alpha, 0));
        for y in 0..2usize {
            for x in 0..3usize {
                let start = (y * 3 + x) * channels * 4;
                let gx = i64::from(display.x) + x as i64;
                let gy = i64::from(display.y) + y as i64;
                if gx < i64::from(data.x)
                    || gx >= i64::from(data.x) + 2
                    || gy < i64::from(data.y)
                    || gy >= i64::from(data.y) + 2
                {
                    assert!(actual[start..start + channels * 4].iter().all(|&b| b == 0));
                }
            }
        }
    }
    let extreme = selection(
        window(i32::MIN, i32::MAX, 1, 1),
        window(i32::MAX, i32::MIN, 1, 1),
        true,
        0,
    );
    assert_eq!(display_pixel(extreme, 0, 0), None);
    assert_eq!(display_pixel(extreme, usize::MAX, usize::MAX), None);
}

#[test]
fn image_decode_exr_adapter_multipart_uses_first_rgb_and_its_alpha() {
    let display = window(0, 0, 2, 1);
    let attrs = ImageAttributes::new(IntegerBounds::from_dimensions((2, 1)));
    let make = |name: &str, channels: AnyChannels<FlatSamples>| {
        Layer::new(
            (2, 1),
            LayerAttributes::named(name),
            Encoding::FAST_LOSSLESS,
            channels,
        )
    };
    for alpha in [false, true] {
        let names: &[&str] = if alpha {
            &["R", "G", "B", "A"]
        } else {
            &["R", "G", "B"]
        };
        let image = exr::image::Image::empty(attrs.clone())
            .with_layer(make("luma", flat_channels(&["Y"], &[vec![0.7; 2]])))
            .with_layer(make(
                "selected",
                flat_channels(
                    names,
                    &[vec![0.25; 2], vec![0.5; 2], vec![0.75; 2], vec![0.0; 2]][..names.len()],
                ),
            ))
            .with_layer(make(
                "later",
                flat_channels(
                    &["R", "G", "B", "A"],
                    &[vec![1.0; 2], vec![0.0; 2], vec![0.0; 2], vec![1.0; 2]],
                ),
            ));
        let bytes = encode(&image);
        let selected = selection(display, display, alpha, 1);
        compare(&bytes, selected);
        let cancel = AtomicBool::new(false);
        for mismatch in [
            Selection {
                part_index: 2,
                ..selected
            },
            Selection {
                has_alpha: !alpha,
                ..selected
            },
            Selection {
                data_window: window(1, 0, 2, 1),
                ..selected
            },
        ] {
            assert!(Decoder::new(&bytes, &cancel, |_, _| Ok(mismatch)).is_err());
        }
    }
}

#[test]
fn image_decode_exr_adapter_tiles_mip_and_rip_keep_largest_resolution() {
    use exr::math::RoundingMode;
    let size = Vec2(4, 2);
    let display = window(0, 0, 4, 2);
    let selected = selection(display, display, false, 0);
    let bytes = flat(
        display,
        display,
        &["R", "G", "B"],
        &[vec![0.25; 8], vec![0.5; 8], vec![0.75; 8]],
        Compression::ZIP16,
        Blocks::Tiles(Vec2(2, 1)),
    );
    compare(&bytes, selected);
    for rip in [false, true] {
        let levels: Vec<_> = if rip {
            exr::meta::rip_map_levels(RoundingMode::Down, size)
                .map(|(_, size)| size)
                .collect()
        } else {
            exr::meta::mip_map_levels(RoundingMode::Down, size)
                .map(|(_, size)| size)
                .collect()
        };
        let channels = AnyChannels::sort(
            [("R", 0.25f32), ("G", 0.5), ("B", 0.75)]
                .into_iter()
                .map(|(name, full_value)| {
                    let map_data = levels
                        .iter()
                        .enumerate()
                        .map(|(level, size)| {
                            FlatSamples::F32(vec![
                                if level == 0 { full_value } else { 0.0 };
                                size.area()
                            ])
                        })
                        .collect();
                    let samples = if rip {
                        Levels::Rip {
                            rounding_mode: RoundingMode::Down,
                            level_data: RipMaps {
                                map_data,
                                level_count: Vec2(3, 2),
                            },
                        }
                    } else {
                        Levels::Mip {
                            rounding_mode: RoundingMode::Down,
                            level_data: map_data,
                        }
                    };
                    AnyChannel::new(name, samples)
                })
                .collect(),
        );
        let layer = Layer::new(
            size,
            LayerAttributes::named("levels"),
            Encoding {
                compression: Compression::ZIP16,
                blocks: Blocks::Tiles(Vec2(2, 1)),
                line_order: LineOrder::Increasing,
            },
            channels,
        );
        compare(&encode(&exr::image::Image::from_layer(layer)), selected);
    }
}

#[test]
fn image_decode_exr_adapter_cancel_reader_and_output_contract() {
    let cancel = AtomicBool::new(false);
    let mut input = CancelCursor {
        cursor: Cursor::new(b"abcd".as_slice()),
        cancel: &cancel,
    };
    let mut one = [0];
    assert_eq!(input.read(&mut one).unwrap(), 1);
    cancel.store(true, Ordering::Relaxed);
    assert!(input.read(&mut one).is_err());
    assert!(input.seek(SeekFrom::Start(0)).is_err());
    cancel.store(false, Ordering::Relaxed);
    let display = window(0, 0, 1, 1);
    let bytes = flat(
        display,
        display,
        &["R", "G", "B"],
        &[vec![1.0], vec![0.0], vec![0.0]],
        Compression::RLE,
        Blocks::ScanLines,
    );
    let selected = selection(display, display, false, 0);
    let decoder = Decoder::new(&bytes, &cancel, |_, _| Ok(selected)).unwrap();
    assert!(decoder.read_image(&mut [0; 11]).is_err());
    let decoder = Decoder::new(&bytes, &cancel, |_, _| Ok(selected)).unwrap();
    cancel.store(true, Ordering::Relaxed);
    let error = decoder.read_image(&mut [0; 12]).unwrap_err();
    assert_eq!(
        error.to_string(),
        tr!("图片读取已取消", "Image reading canceled")
    );
    cancel.store(false, Ordering::Relaxed);
    compare(&bytes, selected);
    assert_eq!(DECODER_THREAD_BYTES, 2 * 1024 * 1024);
    let mut pixels = OwnedPixelStorage::new(4);
    assert!(pixels.failure.is_none());
    assert_eq!(pixels.values.capacity(), 4);
    pixels.values.copy_from_slice(&[0.0, -0.0, 0.5, 1.0]);
    let mut copied = [0; 16];
    pixels.copy_to(&mut copied, &cancel).unwrap();
    assert_eq!(&copied[4..8], &(-0.0f32).to_ne_bytes());
    let failed = OwnedPixelStorage::new(usize::MAX);
    assert_eq!(failed.failure, Some(PixelFailure::Allocation));
    assert!(failed.values.is_empty());
    cancel.store(true, Ordering::Relaxed);
    let error = failed.copy_to(&mut [], &cancel).unwrap_err();
    assert_eq!(
        error.to_string(),
        tr!("图片读取已取消", "Image reading canceled")
    );
}
