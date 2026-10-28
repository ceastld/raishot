use super::*;

fn page(start: u32, background: u8, variation: u8) -> Image {
    let (width, height) = (1024u32, 320u32);
    let mut image = Image {
        width,
        height,
        bgra: [background, background, background, 255].repeat(width as usize * height as usize),
        readback_ms: 0.,
    };
    // 仅七像素宽的字列完全落在旧48/33列采样格之间；白页上模拟短字形及行间空白。
    for y in 0..height {
        let world = start + y;
        if world % 28 >= 12 {
            continue;
        }
        let line = world / 28;
        let pattern = line.wrapping_mul(2_654_435_761).rotate_left(7) | 1;
        for x in 13..20 {
            if pattern & (1 << (x - 13)) == 0 {
                continue;
            }
            let offset = ((y * width + x) * 4) as usize;
            image.bgra[offset..offset + 3].fill(24 + variation);
        }
    }
    image
}

fn matched(first: &Image, next: &Image) -> Match {
    locate(
        &Frame::new(first, ScrollAxis::Vertical, 0, first.height),
        &Frame::new(next, ScrollAxis::Vertical, 0, next.height),
        0,
    )
}

#[test]
fn sparse_text_between_sampling_columns_has_unique_integer_scroll() {
    let first = page(0, 255, 0);
    let next = page(37, 255, 0);
    assert!(matches!(matched(&first, &next), Match::Unique(37)));
}

#[test]
fn sparse_text_soft_noise_fallback_does_not_spend_budget_on_blank_overlaps() {
    let first = page(0, 127, 0);
    let next = page(37, 128, 1);
    // 背景跨过量化桶边界，确保没有精确指纹，必须走近似搜索。
    let source = Frame::new(&first, ScrollAxis::Vertical, 0, first.height);
    let incoming = Frame::new(&next, ScrollAxis::Vertical, 0, next.height);
    assert!(incoming
        .hashes
        .iter()
        .all(|hash| !source.hashes.contains(hash)));
    assert!(matches!(locate(&source, &incoming, 0), Match::Unique(37)));
}

#[test]
fn sparse_text_white_area_does_not_make_neighboring_pixels_equivalent() {
    let first = page(0, 255, 0);
    let next = page(28, 255, 0);
    // 字形有十二行相同笔画，±1候选在全画面上的错误率很小，但正文误差显著。
    assert!(matches!(matched(&first, &next), Match::Unique(28)));
}

#[test]
fn sparse_text_different_page_is_not_accepted_despite_white_background() {
    let first = page(0, 255, 0);
    let mut next = page(37, 255, 0);
    for y in 0..next.height {
        for x in 13..20 {
            let offset = ((y * next.width + x) * 4) as usize;
            let value = if next.bgra[offset] == 255 { 24 } else { 255 };
            next.bgra[offset..offset + 3].fill(value);
        }
    }
    assert!(!matches!(matched(&first, &next), Match::Unique(_)));
}

#[test]
fn periodic_text_remains_ambiguous() {
    let mut first = page(0, 255, 0);
    for y in 0..first.height {
        for x in 13..20 {
            let offset = ((y * first.width + x) * 4) as usize;
            first.bgra[offset..offset + 3].fill(if y % 28 < 12 { 24 } else { 255 });
        }
    }
    let next = Image {
        width: first.width,
        height: first.height,
        bgra: first.bgra.clone(),
        readback_ms: 0.,
    };
    assert!(matches!(matched(&first, &next), Match::Ambiguous));
}

#[test]
fn repeated_text_styles_are_not_outvoted_by_a_small_dynamic_icon() {
    let page = |start: u32, version: u32| {
        let (width, height) = (1024u32, 1080u32);
        let mut image = Image {
            width,
            height,
            bgra: [255, 255, 255, 255].repeat(width as usize * height as usize),
            readback_ms: 0.,
        };
        for y in 0..height {
            let world = start + y;
            let style = (world.wrapping_mul(2_654_435_761).rotate_left(world % 31) >> 8) & 7;
            for x in 13..29 {
                if (x - 13 + style) % 16 < 4 {
                    continue;
                }
                let offset = ((y * width + x) * 4) as usize;
                image.bgra[offset..offset + 3].fill(24);
            }
            if (16..80).contains(&y) {
                for x in 100..132 {
                    let offset = ((y * width + x) * 4) as usize;
                    let value = ((y * 37 + x * 11 + version * 61) % 128 + 32) as u8;
                    image.bgra[offset..offset + 3].fill(value);
                }
            }
        }
        image
    };
    let first = page(0, 0);
    let next = page(37, 1);
    let source = Frame::new(&first, ScrollAxis::Vertical, 0, first.height);
    let incoming = Frame::new(&next, ScrollAxis::Vertical, 0, next.height);
    let mut counts = HashMap::new();
    for row in 80..incoming.length() {
        *counts.entry(incoming.hash(row)).or_insert(0u32) += 1;
    }
    assert_eq!(counts.len(), 8);
    assert!(counts.values().all(|&count| count > 64));
    assert!(score(&source, &incoming, 37, true).is_some());
    assert!(probe_match(
        &source,
        &incoming,
        37,
        &content_probes(&incoming)
    ));
    assert!(matches!(locate(&source, &incoming, 0), Match::Unique(37)));
}

#[test]
fn right_edge_text_soft_noise_uses_content_probes_instead_of_exhausting_budget() {
    let at_right = |mut image: Image| {
        for y in 0..image.height {
            for x in 13..20 {
                let from = ((y * image.width + x) * 4) as usize;
                let to = ((y * image.width + image.width - 7 + x - 13) * 4) as usize;
                let pixel: [u8; 4] = image.bgra[from..from + 4].try_into().unwrap();
                let background = image.bgra[(y * image.width * 4) as usize];
                image.bgra[to..to + 4].copy_from_slice(&pixel);
                image.bgra[from..from + 4]
                    .copy_from_slice(&[background, background, background, 255]);
            }
        }
        image
    };
    let first = at_right(page(0, 127, 0));
    let next = at_right(page(37, 128, 1));
    let source = Frame::new(&first, ScrollAxis::Vertical, 0, first.height);
    let incoming = Frame::new(&next, ScrollAxis::Vertical, 0, next.height);
    assert!(incoming
        .hashes
        .iter()
        .all(|hash| !source.hashes.contains(hash)));
    let probes = content_probes(&incoming);
    assert!(probes.len() >= 8);
    assert!(probes
        .iter()
        .all(|probe| probe.columns.iter().all(|&x| x >= first.width - 8)));
    assert!(matches!(locate(&source, &incoming, 0), Match::Unique(37)));
}
