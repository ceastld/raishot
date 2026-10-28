use super::*;

fn rect(left: i32, top: i32, width: u32, height: u32) -> ScreenRect {
    ScreenRect {
        left,
        top,
        width,
        height,
    }
}

#[test]
fn client_region_clips_selection_and_preserves_negative_desktop_origin() {
    assert_eq!(
        client_region(rect(-1900, -100, 700, 400), rect(-1800, 0, 600, 600)),
        Some(rect(0, 0, 600, 300))
    );
    assert_eq!(
        client_region(rect(600, 0, 20, 20), rect(0, 0, 600, 600)),
        None
    );
}

#[test]
fn source_region_follows_client_origin_without_changing_locked_region() {
    let region = rect(40, 60, 100, 80);
    let first = source_region(
        region,
        rect(-1900, 132, 600, 500),
        rect(-1908, 100, 616, 540),
        rect(-1900, 100, 600, 532),
        (616, 540),
    );
    let moved = source_region(
        region,
        rect(100, 232, 600, 500),
        rect(92, 200, 616, 540),
        rect(100, 200, 600, 532),
        (616, 540),
    );
    assert_eq!(first, Some(rect(48, 92, 100, 80)));
    assert_eq!(moved, first);
}

#[test]
fn source_region_accepts_visible_frame_and_rejects_unknown_size_or_shrunk_client() {
    let region = rect(40, 60, 100, 80);
    assert_eq!(
        source_region(
            region,
            rect(8, 32, 600, 500),
            rect(0, 0, 616, 540),
            rect(8, 0, 600, 532),
            (600, 532)
        ),
        Some(rect(40, 92, 100, 80))
    );
    assert_eq!(
        source_region(
            region,
            rect(8, 32, 600, 500),
            rect(0, 0, 616, 540),
            rect(8, 0, 600, 532),
            (300, 266)
        ),
        None
    );
    assert_eq!(
        source_region(
            region,
            rect(8, 32, 80, 80),
            rect(0, 0, 96, 120),
            rect(8, 0, 80, 112),
            (96, 120)
        ),
        None
    );
}

#[test]
fn zoom_keeps_pointer_anchor_and_obeys_texture_budget() {
    let initial = Placement {
        x: -1000,
        y: -200,
        width: 400,
        height: 200,
    };
    let (zoomed, zoom) = zoom_placement(initial, (400, 200), 2., (-900, -150));
    assert_eq!(zoom, 2.);
    assert_eq!(
        zoomed,
        Placement {
            x: -1100,
            y: -250,
            width: 800,
            height: 400
        }
    );
    let (bounded, _) = zoom_placement(initial, (8000, 4000), 5., (-900, -150));
    assert!(bounded.width <= 8192 && bounded.height <= 8192);
    assert!(u64::from(bounded.width) * u64::from(bounded.height) <= 8_000_000);
}
