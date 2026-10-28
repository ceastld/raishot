use super::*;

#[test]
fn image_viewer_animation_resident_all_sources_reject_before_path_and_cancel_wins() {
    for kind in [Kind::Gif, Kind::Apng, Kind::Webp] {
        for cancelled in [false, true] {
            let error = AnyAnimation::open(
                kind,
                Path::new(""),
                crate::image_decode::MAX_DECODE_BYTES,
                Arc::new(AtomicBool::new(cancelled)),
                ms(0),
            )
            .err()
            .expect("resident or cancellation must fail before an empty path is accessed");
            assert_eq!(
                error,
                if cancelled {
                    tr!("动画加载已取消", "Animation loading canceled").to_string()
                } else {
                    resident_error()
                }
            );
        }
    }
}

fn budget_gif(width: u16) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut bytes, width, 1, &[255, 0, 0, 0, 0, 255]).unwrap();
        encoder.set_repeat(gif::Repeat::Infinite).unwrap();
        for color in [0, 1] {
            encoder
                .write_frame(&gif::Frame {
                    width,
                    height: 1,
                    delay: 1,
                    dispose: gif::DisposalMethod::Keep,
                    buffer: Cow::Owned(vec![color; usize::from(width)]),
                    ..Default::default()
                })
                .unwrap();
        }
    }
    bytes
}

#[test]
fn image_viewer_animation_resident_gif_exact_allowance_keeps_pixels_restart_and_long_tiles() {
    for width in [2u16, 20_000] {
        let bytes = budget_gif(width);
        let cancel = Arc::new(AtomicBool::new(false));
        let source =
            GifSource::from_reader(Cursor::new(&bytes), GifOptions::default(), cancel.clone())
                .unwrap();
        let core = source.reserved_bytes();
        let additional = u64::from(width) * if width > 16_384 { 16 } else { 12 };
        let resident = crate::image_decode::MAX_DECODE_BYTES
            - core
            - additional
            - super::super::super::THUMBNAIL_CACHE_BYTES as u64
            - super::super::super::THUMBNAIL_WORKING_BYTES;
        drop(source);
        let options = gif_options(resident, &cancel).unwrap();
        let mut playback =
            GifPlayback::from_reader(Cursor::new(&bytes), options, cancel.clone(), true).unwrap();
        check_gif_presentation_budget(playback.dimensions(), core, options.max_total_bytes)
            .unwrap();
        assert!(check_gif_presentation_budget(
            playback.dimensions(),
            core,
            gif_options(resident + 1, &cancel).unwrap().max_total_bytes
        )
        .is_err());

        playback.restart(ms(0), true).unwrap();
        assert_eq!(playback.source().reserved_bytes(), core);
        let (animation, image) = ActiveAnimation::initialize(playback, None, ms(0)).unwrap();
        let first = image.raw_frame.as_ref().unwrap().image();
        assert_eq!((first.width, first.height), (u32::from(width), 1));
        assert_eq!(first.bgra, [0, 0, 255, 255].repeat(usize::from(width)));
        assert_eq!(animation.playback.source().reserved_bytes(), core);
        assert_eq!(image.raw_frame.as_ref().unwrap().generation(), 1);
        assert_eq!(
            image.tiles.iter().map(|(rect, _)| rect.width).sum::<u32>(),
            u32::from(width)
        );
        assert_eq!(image.tiles.len() > 1, width > 16_384);
    }
}
