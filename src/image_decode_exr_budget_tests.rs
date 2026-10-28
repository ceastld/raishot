use super::*;

fn attr(out: &mut Vec<u8>, name: &[u8], kind: &[u8], data: &[u8]) {
    out.extend_from_slice(name);
    out.push(0);
    out.extend_from_slice(kind);
    out.push(0);
    out.extend_from_slice(&(data.len() as i32).to_le_bytes());
    out.extend_from_slice(data);
}

fn box_bytes(window: Window) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in [
        window.x,
        window.y,
        window.x + window.width as i32 - 1,
        window.y + window.height as i32 - 1,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn channel_bytes(alpha: bool, rgb: bool) -> Vec<u8> {
    let names: &[&[u8]] = if !rgb {
        &[b"Y"]
    } else if alpha {
        &[b"A", b"B", b"G", b"R"]
    } else {
        &[b"B", b"G", b"R"]
    };
    let mut bytes = Vec::new();
    for name in names {
        bytes.extend_from_slice(name);
        bytes.push(0);
        bytes.extend_from_slice(&1u32.to_le_bytes()); // HALF
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.extend_from_slice(&1i32.to_le_bytes());
    }
    bytes.push(0);
    bytes
}

fn header(
    data: Window,
    display: Window,
    alpha: bool,
    rgb: bool,
    tiled: Option<(u32, u32, u8)>,
    extra: &[u8],
    multipart: bool,
) -> Vec<u8> {
    let mut out = Vec::new();
    attr(&mut out, b"channels", b"chlist", &channel_bytes(alpha, rgb));
    attr(&mut out, b"compression", b"compression", &[0]);
    attr(&mut out, b"dataWindow", b"box2i", &box_bytes(data));
    attr(&mut out, b"displayWindow", b"box2i", &box_bytes(display));
    attr(&mut out, b"lineOrder", b"lineOrder", &[0]);
    attr(&mut out, b"pixelAspectRatio", b"float", &1f32.to_le_bytes());
    attr(&mut out, b"screenWindowCenter", b"v2f", &[0; 8]);
    attr(
        &mut out,
        b"screenWindowWidth",
        b"float",
        &1f32.to_le_bytes(),
    );
    if let Some((width, height, mode)) = tiled {
        let mut tile = Vec::new();
        tile.extend_from_slice(&width.to_le_bytes());
        tile.extend_from_slice(&height.to_le_bytes());
        tile.push(mode);
        attr(&mut out, b"tiles", b"tiledesc", &tile);
    }
    if multipart {
        attr(
            &mut out,
            b"name",
            b"string",
            if rgb { b"color" } else { b"depth" },
        );
        attr(
            &mut out,
            b"type",
            b"string",
            if tiled.is_some() {
                b"tiledimage"
            } else {
                b"scanlineimage"
            },
        );
    }
    out.extend_from_slice(extra);
    out.push(0);
    out
}

fn scan_chunk(part: Option<u32>, y: i32, raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(part) = part {
        out.extend_from_slice(&part.to_le_bytes());
    }
    out.extend_from_slice(&y.to_le_bytes());
    out.extend_from_slice(&(raw.len() as i32).to_le_bytes());
    out.extend_from_slice(raw);
    out
}

fn tile_chunk(x: u32, y: u32, lx: u32, ly: u32, raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in [x, y, lx, ly] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&(raw.len() as i32).to_le_bytes());
    out.extend_from_slice(raw);
    out
}

fn container(headers: &[Vec<u8>], flags: u32, chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![0x76, 0x2f, 0x31, 0x01];
    out.extend_from_slice(&(2 | flags).to_le_bytes());
    for header in headers {
        out.extend_from_slice(header);
    }
    if flags & 0x1000 != 0 {
        out.push(0);
    }
    let table = out.len();
    out.resize(table + chunks.len() * 8, 0);
    for (index, chunk) in chunks.iter().enumerate() {
        let offset = out.len() as u64;
        out[table + index * 8..table + (index + 1) * 8].copy_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out
}

fn unit() -> Window {
    Window {
        x: 0,
        y: 0,
        width: 1,
        height: 1,
    }
}

fn one(extra: &[u8]) -> Vec<u8> {
    container(
        &[header(unit(), unit(), false, true, None, extra, false)],
        0,
        &[scan_chunk(None, 0, &[0, 0x3c, 0, 0, 0, 0])],
    )
}

fn inspect_local(bytes: &[u8]) -> ExrWorkspace<'_> {
    inspect(bytes, bytes.len() as u64, &AtomicBool::new(false)).unwrap()
}

#[test]
fn image_decode_exr_directory_selects_first_flat_rgb_and_borrows_real_channels() {
    let data = Window {
        x: -3,
        y: -2,
        width: 2,
        height: 1,
    };
    let display = Window {
        x: -4,
        y: -3,
        width: 4,
        height: 3,
    };
    let source = container(
        &[
            header(unit(), unit(), false, false, None, &[], true),
            header(data, display, true, true, None, &[], true),
        ],
        0x1000,
        &[
            scan_chunk(Some(0), 0, &[0; 2]),
            scan_chunk(Some(1), -2, &[0; 16]),
        ],
    );
    let plan = inspect_local(&source);
    assert_eq!(plan.header_count, 2);
    assert_eq!(plan.total_chunk_count, 2);
    assert_eq!(plan.selection.part_index, 1);
    assert_eq!(plan.selection.data_window, data);
    assert_eq!(plan.selection.display_window, display);
    assert!(plan.selection.has_alpha);
    assert_eq!(plan.native_bytes, 4 * 3 * 16);
    let channels = plan.channels().iter().collect::<Vec<_>>();
    assert_eq!(channels.len(), 4);
    assert_eq!(channels[0].name, b"A");
    assert_eq!(channels[3].name, b"R");
    assert_eq!(channels[0].sample_type, 1);
    assert_eq!((channels[0].sampling_x, channels[0].sampling_y), (1, 1));
    let mut visited = 0;
    plan.for_each_selected_chunk(&AtomicBool::new(false), |chunk| {
        visited += 1;
        assert_eq!(chunk.dimensions, (2, 1));
        assert_eq!(chunk.origin_y, -2);
        assert_eq!(chunk.pixel_origin, (0, 0));
        assert_eq!(chunk.raw_block_bytes, 16);
        assert_eq!(
            chunk.payload.as_ptr(),
            source[chunk.offset as usize + 12..].as_ptr()
        );
        Ok(())
    })
    .unwrap();
    assert_eq!(visited, 1);
    assert_eq!(plan.compression, CompressionWorkspace::Pending);
}

#[test]
fn image_decode_exr_directory_preserves_tiles_mips_rips_and_unread_offsets() {
    let data = Window {
        width: 3,
        height: 2,
        ..unit()
    };
    for (mode, levels) in [
        (0, vec![(0, 0, 3, 2)]),
        (0x11, vec![(0, 0, 3, 2), (1, 1, 2, 1), (2, 2, 1, 1)]),
        (
            0x12,
            vec![
                (0, 0, 3, 2),
                (1, 0, 2, 2),
                (2, 0, 1, 2),
                (0, 1, 3, 1),
                (1, 1, 2, 1),
                (2, 1, 1, 1),
            ],
        ),
    ] {
        let mut chunks = Vec::new();
        for (lx, ly, width, height) in levels {
            for x in 0..(width + 1) / 2 {
                let tile_width = 2.min(width - x * 2);
                chunks.push(tile_chunk(
                    x,
                    0,
                    lx,
                    ly,
                    &vec![0; (tile_width * height * 6) as usize],
                ));
            }
        }
        let mut source = container(
            &[header(
                data,
                data,
                false,
                true,
                Some((2, 2, mode)),
                &[],
                false,
            )],
            0x200,
            &chunks,
        );
        let plan = inspect_local(&source);
        assert_eq!(plan.selection.selected_chunk_count, 2);
        assert_eq!(plan.selection.chunk_count, chunks.len());
        let table = plan.selection.offset_table_start;
        if chunks.len() > 2 {
            // 后端读取全目录但不访问lower level offset；不把此值当有效像素范围要求。
            source[table + 16..table + 24].fill(0);
        }
        let plan = inspect_local(&source);
        let mut dimensions = Vec::new();
        plan.for_each_selected_chunk(&AtomicBool::new(false), |chunk| {
            dimensions.push(chunk.dimensions);
            Ok(())
        })
        .unwrap();
        assert_eq!(dimensions, vec![(2, 2), (1, 2)]);
    }
    let mut source = container(
        &[
            header(unit(), unit(), false, false, None, &[], true),
            header(unit(), unit(), false, true, None, &[], true),
        ],
        0x1000,
        &[
            scan_chunk(Some(0), 0, &[0; 2]),
            scan_chunk(Some(1), 0, &[0; 6]),
        ],
    );
    let table = inspect_local(&source).selection.offset_table_start - 8;
    source[table..table + 8].fill(0);
    assert_eq!(inspect_local(&source).selection.part_index, 1);
}

#[test]
fn image_decode_exr_directory_uses_computed_count_and_checks_real_selected_leaders() {
    let mut extra = Vec::new();
    attr(&mut extra, b"chunkCount", b"int", &i32::MAX.to_le_bytes());
    let source = one(&extra);
    let plan = inspect_local(&source);
    assert_eq!(plan.selection.chunk_count, 1);
    let table = plan.selection.offset_table_start;
    let offset = u64_at(&source, table).unwrap() as usize;
    for replacement in [0u64, source.len() as u64, u64::MAX] {
        let mut bad = source.clone();
        bad[table..table + 8].copy_from_slice(&replacement.to_le_bytes());
        assert!(inspect(&bad, bad.len() as u64, &AtomicBool::new(false)).is_err());
    }
    let mut bad = source.clone();
    bad[offset..offset + 4].copy_from_slice(&1i32.to_le_bytes());
    assert!(inspect(&bad, bad.len() as u64, &AtomicBool::new(false)).is_err());
    let mut bad = source.clone();
    bad[offset + 4..offset + 8].copy_from_slice(&7i32.to_le_bytes());
    assert!(inspect(&bad, bad.len() as u64, &AtomicBool::new(false)).is_err());
    let mut bad = source.clone();
    bad.pop();
    assert!(inspect(&bad, bad.len() as u64, &AtomicBool::new(false)).is_err());

    let mut multi = container(
        &[
            header(unit(), unit(), false, false, None, &[], true),
            header(unit(), unit(), false, true, None, &[], true),
        ],
        0x1000,
        &[
            scan_chunk(Some(0), 0, &[0; 2]),
            scan_chunk(Some(1), 0, &[0; 6]),
        ],
    );
    let selected = inspect_local(&multi).selection;
    let offset = u64_at(&multi, selected.offset_table_start).unwrap() as usize;
    multi[offset..offset + 4].copy_from_slice(&0u32.to_le_bytes());
    assert!(inspect(&multi, multi.len() as u64, &AtomicBool::new(false)).is_err());
}

#[test]
fn image_decode_exr_directory_repeated_and_reversed_offsets_use_real_geometry() {
    let data = Window {
        width: 3,
        height: 2,
        ..unit()
    };
    let original = container(
        &[header(data, data, false, true, Some((2, 2, 0)), &[], false)],
        0x200,
        &[
            tile_chunk(0, 0, 0, 0, &[11; 24]),
            tile_chunk(1, 0, 0, 0, &[22; 12]),
        ],
    );
    let table = inspect_local(&original).selection.offset_table_start;
    let first = u64_at(&original, table).unwrap();
    let second = u64_at(&original, table + 8).unwrap();
    for (offsets, expected) in [
        (
            [second, first],
            vec![((2, 0), (1, 2), 12, 22), ((0, 0), (2, 2), 24, 11)],
        ),
        (
            [second, second],
            vec![((2, 0), (1, 2), 12, 22), ((2, 0), (1, 2), 12, 22)],
        ),
    ] {
        let mut bytes = original.clone();
        bytes[table..table + 8].copy_from_slice(&offsets[0].to_le_bytes());
        bytes[table + 8..table + 16].copy_from_slice(&offsets[1].to_le_bytes());
        let plan = inspect_local(&bytes);
        let mut seen = Vec::new();
        plan.for_each_selected_chunk(&AtomicBool::new(false), |chunk| {
            assert_eq!(chunk.table_index, seen.len());
            seen.push((
                chunk.pixel_origin,
                chunk.dimensions,
                chunk.raw_block_bytes,
                chunk.payload[0],
            ));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, expected);
    }
    for (field, value) in [(0, -1i32), (0, 2), (8, 1), (12, 1)] {
        let mut bytes = original.clone();
        bytes[first as usize + field..first as usize + field + 4]
            .copy_from_slice(&value.to_le_bytes());
        assert!(inspect(&bytes, bytes.len() as u64, &AtomicBool::new(false)).is_err());
    }

    // ZIP16允许未压缩raw fallback；两个真实块是16行与最后1行。
    let data = Window {
        width: 1,
        height: 17,
        ..unit()
    };
    let mut extra = Vec::new();
    attr(&mut extra, b"compression", b"compression", &[3]);
    let original = container(
        &[header(data, data, false, true, None, &extra, false)],
        0,
        &[
            scan_chunk(None, 0, &[33; 96]),
            scan_chunk(None, 16, &[44; 6]),
        ],
    );
    let table = inspect_local(&original).selection.offset_table_start;
    let first = u64_at(&original, table).unwrap();
    let second = u64_at(&original, table + 8).unwrap();
    for (offsets, expected) in [
        ([second, first], vec![(16, 6, 44), (0, 96, 33)]),
        ([first, first], vec![(0, 96, 33), (0, 96, 33)]),
    ] {
        let mut bytes = original.clone();
        bytes[table..table + 8].copy_from_slice(&offsets[0].to_le_bytes());
        bytes[table + 8..table + 16].copy_from_slice(&offsets[1].to_le_bytes());
        let plan = inspect_local(&bytes);
        let mut seen = Vec::new();
        plan.for_each_selected_chunk(&AtomicBool::new(false), |chunk| {
            seen.push((chunk.origin_y, chunk.raw_block_bytes, chunk.payload[0]));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, expected);
    }
    // 后端带符号除法向0截断，不额外强制leader对齐；-1/16与1/16均映射首块。
    for leader in [-1i32, 1] {
        let mut bytes = original.clone();
        bytes[first as usize..first as usize + 4].copy_from_slice(&leader.to_le_bytes());
        let plan = inspect_local(&bytes);
        let mut rows = Vec::new();
        plan.for_each_selected_chunk(&AtomicBool::new(false), |chunk| {
            rows.push(chunk.origin_y);
            Ok(())
        })
        .unwrap();
        assert_eq!(rows, [0, 16]);
    }
    for leader in [-16i32, 32] {
        let mut bytes = original.clone();
        bytes[first as usize..first as usize + 4].copy_from_slice(&leader.to_le_bytes());
        assert!(inspect(&bytes, bytes.len() as u64, &AtomicBool::new(false)).is_err());
    }
}

#[test]
fn image_decode_exr_metadata_counts_typed_expansion_raw_overlap_and_preview_eof_step() {
    let simple = one(&[]);
    let baseline = inspect_local(&simple);
    let mut extra = Vec::new();
    let vector = vec![0; 4000]; // 1000个空Text，不是一个4000B字符块。
    attr(&mut extra, b"viewNames", b"stringvector", &vector);
    attr(&mut extra, b"custom", b"unrecognized", &[42; 257]);
    let expanded_source = one(&extra);
    let expanded = inspect_local(&expanded_source);
    assert!(
        expanded.retained_metadata_bytes
            > baseline.retained_metadata_bytes
                + 1000 * size_of::<exr::meta::attribute::Text>() as u64
    );
    assert!(expanded.constructor_bytes > expanded.retained_metadata_bytes + vector.len() as u64);
    assert!(expanded.filter_extra_bytes >= 2 * expanded.retained_metadata_bytes);

    let mut preview = Vec::new();
    preview.extend_from_slice(&1_000_000u32.to_le_bytes());
    preview.extend_from_slice(&1_000_000u32.to_le_bytes());
    let mut extra = Vec::new();
    attr(&mut extra, b"preview", b"preview", &preview);
    let source = one(&extra);
    let plan = inspect_local(&source);
    // 此typed属性被宽容后端忽略，但EOF发生前仍先分配一个4MiB读取步长。
    assert!(plan.constructor_bytes >= 3 * PREVIEW_STEP);
    assert_eq!(plan.selection.selected_chunk_count, 1);
    let mut truncated = simple[..8].to_vec();
    truncated.extend_from_slice(b"huge\0custom\0");
    truncated.extend_from_slice(&i32::MAX.to_le_bytes());
    assert!(inspect(&truncated, truncated.len() as u64, &AtomicBool::new(false)).is_err());
}

#[test]
fn image_decode_exr_directory_budget_boundaries_and_cancel_are_explicit() {
    let source = one(&[]);
    let plan = inspect_local(&source);
    let required = plan.constructor_bytes.max(
        plan.retained_metadata_bytes
            + plan.native_bytes
            + plan.filter_extra_bytes
            + plan.decoder_thread_bytes,
    );
    let exact = MAX_BYTES - required;
    assert!(inspect(&source, exact, &AtomicBool::new(false)).is_ok());
    assert!(inspect(&source, exact + 1, &AtomicBool::new(false)).is_err());
    assert!(inspect(&source, source.len() as u64 - 1, &AtomicBool::new(false)).is_err());
    let error = inspect(&[], u64::MAX, &AtomicBool::new(true))
        .unwrap_err()
        .to_string();
    assert!(error == tr!("图片读取已取消", "Image reading canceled"));
    let cancel = AtomicBool::new(false);
    let error = plan
        .for_each_selected_chunk(&cancel, |_| {
            cancel.store(true, Ordering::Relaxed);
            Err("callback failure".into())
        })
        .unwrap_err()
        .to_string();
    assert_eq!(error, tr!("图片读取已取消", "Image reading canceled"));
}
