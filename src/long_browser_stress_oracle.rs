//! 浏览器 32k 专项的独立物理像素合同；逐行借用条带，不物化第二份整图。
use crate::{
    capture::Image,
    stitch::{ScrollAxis, StitchSegment, StitchSnapshot},
    Result,
};
use serde::Serialize;
use std::{collections::HashSet, mem::size_of, sync::Arc};

const TOLERANCE: u8 = 1;
const MAX_PIXELS: u64 = 32_000_000;

#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Spec {
    pub(crate) dpr: f64,
    pub(crate) width: u32,
    pub(crate) viewport_height: u32,
    pub(crate) document_height: u32,
    pub(crate) tile_height: u32,
}
impl Spec {
    pub(crate) fn new(dpr: f64) -> Result<Self> {
        ensure(
            dpr.is_finite() && dpr == 1.25,
            "浏览器32k专项须使用实测DPR1.25",
        )?;
        let spec = Self::physical();
        ensure(
            u64::from(spec.width) * u64::from(spec.document_height) <= MAX_PIXELS,
            "浏览器32k物理尺寸超过3200万像素",
        )?;
        Ok(spec)
    }
    const fn physical() -> Self {
        Self {
            dpr: 1.25,
            width: 960,
            viewport_height: 640,
            document_height: 32_000,
            tile_height: 1000,
        }
    }
    pub(crate) fn pixel(self, x: u32, world_y: u32) -> [u8; 4] {
        let band = world_y / self.tile_height;
        let row = world_y % self.tile_height;
        // 独立 literal，与页面只共享公开规格，不读取页面输出作为期望。
        let rgba = if x < 12 && row < 12 {
            Some([251, 19, 73, 255])
        } else if x >= self.width - 12 && row < 12 {
            Some([17, 239, 101, 255])
        } else if x < 12 && row >= self.tile_height - 12 {
            Some([37, 89, 247, 255])
        } else if x >= self.width - 12 && row >= self.tile_height - 12 {
            Some([233, 181, 29, 255])
        } else if x >= 12 && x < self.width - 12 && row < 2 {
            Some([
                (32 + band.wrapping_mul(31) % 192) as u8,
                (32 + band.wrapping_mul(47) % 192) as u8,
                (32 + band.wrapping_mul(29) % 192) as u8,
                255,
            ])
        } else {
            None
        };
        if let Some([r, g, b, a]) = rgba {
            return [b, g, r, a];
        }
        let noise = world_y.wrapping_mul(157).wrapping_add(x.wrapping_mul(71))
            ^ world_y.wrapping_mul(17).wrapping_add(x.wrapping_mul(113));
        [
            (noise % 251) as u8,
            (world_y.wrapping_mul(13).wrapping_add(x.wrapping_mul(19)) % 251) as u8,
            (world_y
                .wrapping_mul(world_y)
                .wrapping_add(x.wrapping_mul(41))
                % 251) as u8,
            255,
        ]
    }
}
fn ensure(value: bool, message: &str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn image_bytes(image: &Image) -> Result<u64> {
    let pixels = u64::from(image.width) * u64::from(image.height);
    let bytes = pixels.checked_mul(4).ok_or("源图像素字节溢出")?;
    ensure(
        image.width > 0
            && image.height > 0
            && image.width <= 65_535
            && image.height <= 65_535
            && pixels <= MAX_PIXELS
            && image.bgra.len() as u64 == bytes,
        "源图尺寸/实际BGRA长度无效或超过3200万像素",
    )?;
    Ok(bytes)
}
fn ordered(snapshot: &StitchSnapshot) -> Result<Vec<&StitchSegment>> {
    let spec = Spec::physical();
    ensure(
        snapshot.axis == ScrollAxis::Vertical
            && snapshot.width == spec.width
            && (spec.viewport_height..=spec.document_height).contains(&snapshot.height)
            && u64::from(snapshot.width) * u64::from(snapshot.height) <= MAX_PIXELS
            && snapshot.viewport.x == 0
            && snapshot.viewport.width == spec.width
            && snapshot.viewport.height == spec.viewport_height
            && snapshot.viewport.valid(snapshot.width, snapshot.height),
        "浏览器32k快照尺寸/轴/viewport无效",
    )?;
    let mut rows: Vec<_> = snapshot.segments.iter().collect();
    rows.sort_unstable_by_key(|segment| segment.origin.1);
    let mut cursor = 0;
    for segment in &rows {
        image_bytes(&segment.image)?;
        ensure(
            segment
                .source
                .valid(segment.image.width, segment.image.height)
                && segment.origin.0 == 0
                && segment.origin.1 == cursor
                && segment.source.width == snapshot.width,
            "浏览器长图条带sourceRect非法、存在缺口/重叠或不覆盖完整宽度",
        )?;
        cursor = cursor
            .checked_add(segment.source.height)
            .ok_or("条带累计行数溢出")?;
        ensure(cursor <= snapshot.height, "浏览器条带超出成品末尾")?;
    }
    ensure(
        !rows.is_empty() && cursor == snapshot.height,
        "浏览器条带未覆盖全部成品",
    )?;
    Ok(rows)
}
fn row(segment: &StitchSegment, output_y: u32) -> &[u8] {
    let y = segment.source.y + output_y - segment.origin.1;
    let start = (y as usize * segment.image.width as usize + segment.source.x as usize) * 4;
    &segment.image.bgra[start..start + segment.source.width as usize * 4]
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct PixelCheck {
    pub(crate) pixels: u64,
    pub(crate) channels: u64,
    pub(crate) max_channel_difference: u8,
    pub(crate) tolerance: u8,
}
fn check(actual: &[u8], expected: [u8; 4], x: u32, y: u32, maximum: &mut u8) -> Result<()> {
    ensure(
        actual[3] == 255 && expected[3] == 255,
        "浏览器32k内容出现透明像素",
    )?;
    for channel in 0..4 {
        let difference = actual[channel].abs_diff(expected[channel]);
        *maximum = (*maximum).max(difference);
        if difference > TOLERANCE {
            return Err(format!("浏览器32k像素不符：({x},{y}) channel={channel} actual={actual:?} expected={expected:?} diff={difference} limit={TOLERANCE}").into());
        }
    }
    Ok(())
}
fn pixel_check(pixels: u64, maximum: u8) -> PixelCheck {
    PixelCheck {
        pixels,
        channels: pixels * 4,
        max_channel_difference: maximum,
        tolerance: TOLERANCE,
    }
}
pub(crate) fn verify_image(image: &Image, first_world_y: u32) -> Result<PixelCheck> {
    image_bytes(image)?;
    let spec = Spec::physical();
    ensure(
        image.width == spec.width
            && image.height <= spec.viewport_height
            && first_world_y
                .checked_add(image.height)
                .is_some_and(|end| end <= spec.document_height),
        "浏览器renderer/viewport世界坐标范围无效",
    )?;
    let mut maximum = 0;
    for y in 0..image.height {
        for x in 0..image.width {
            let index = (y as usize * image.width as usize + x as usize) * 4;
            check(
                &image.bgra[index..index + 4],
                spec.pixel(x, first_world_y + y),
                x,
                first_world_y + y,
                &mut maximum,
            )?;
        }
    }
    Ok(pixel_check(
        u64::from(image.width) * u64::from(image.height),
        maximum,
    ))
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct SnapshotCheck {
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) first_world_y: u32,
    pub(crate) row_count: u32,
    pub(crate) segment_count: usize,
    pub(crate) ordered_metadata_bytes: usize,
    pub(crate) reference_bgra_bytes: usize,
    pub(crate) pixel_check: PixelCheck,
}
pub(crate) fn verify_snapshot(
    snapshot: &StitchSnapshot,
    first_world_y: u32,
) -> Result<SnapshotCheck> {
    let rows = ordered(snapshot)?;
    let spec = Spec::physical();
    ensure(
        first_world_y
            .checked_add(snapshot.height)
            .is_some_and(|end| end <= spec.document_height),
        "浏览器快照世界范围超过32000行",
    )?;
    let mut maximum = 0;
    for segment in &rows {
        for y in segment.origin.1..segment.origin.1 + segment.source.height {
            let pixels = row(segment, y);
            for (x, actual) in pixels.chunks_exact(4).enumerate() {
                check(
                    actual,
                    spec.pixel(x as u32, first_world_y + y),
                    x as u32,
                    first_world_y + y,
                    &mut maximum,
                )?;
            }
        }
    }
    Ok(SnapshotCheck {
        width: snapshot.width,
        height: snapshot.height,
        first_world_y,
        row_count: snapshot.height,
        segment_count: rows.len(),
        ordered_metadata_bytes: rows.capacity() * size_of::<&StitchSegment>(),
        reference_bgra_bytes: 0,
        pixel_check: pixel_check(
            u64::from(snapshot.width) * u64::from(snapshot.height),
            maximum,
        ),
    })
}
pub(crate) fn compare_viewport(snapshot: &StitchSnapshot, renderer: &Image) -> Result<PixelCheck> {
    let rows = ordered(snapshot)?;
    image_bytes(renderer)?;
    ensure(
        renderer.width == snapshot.viewport.width && renderer.height == snapshot.viewport.height,
        "renderer实际像素尺寸与成品viewport不等",
    )?;
    let mut maximum = 0;
    let mut index = 0;
    for y in 0..renderer.height {
        let output_y = snapshot.viewport.y + y;
        while index + 1 < rows.len() && rows[index + 1].origin.1 <= output_y {
            index += 1;
        }
        let actual = row(rows[index], output_y);
        let start = y as usize * renderer.width as usize * 4;
        let expected = &renderer.bgra[start..start + renderer.width as usize * 4];
        for (x, (a, e)) in actual
            .chunks_exact(4)
            .zip(expected.chunks_exact(4))
            .enumerate()
        {
            check(
                a,
                [e[0], e[1], e[2], e[3]],
                x as u32,
                output_y,
                &mut maximum,
            )?;
        }
    }
    Ok(pixel_check(
        u64::from(renderer.width) * u64::from(renderer.height),
        maximum,
    ))
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct SourceAllocation {
    pub(crate) unique_sources: usize,
    pub(crate) unique_capacity_bytes: u64,
    pub(crate) used_pixel_bytes: u64,
    pub(crate) effective_result_bytes: u64,
    pub(crate) excess_capacity_bytes: u64,
    pub(crate) segment_count: usize,
    pub(crate) metadata_capacity_bytes: u64,
    pub(crate) image_struct_bytes: u64,
    pub(crate) arc_header_bytes_estimate: u64,
    pub(crate) total_process_memory: bool,
}
pub(crate) fn source_allocations(snapshot: &StitchSnapshot) -> Result<SourceAllocation> {
    let rows = ordered(snapshot)?;
    let mut sources = HashSet::new();
    let (mut capacity, mut used) = (0_u64, 0_u64);
    for segment in rows {
        if sources.insert(Arc::as_ptr(&segment.image) as usize) {
            capacity = capacity
                .checked_add(segment.image.bgra.capacity() as u64)
                .ok_or("源图capacity字节溢出")?;
            used = used
                .checked_add(image_bytes(&segment.image)?)
                .ok_or("源图实际字节溢出")?;
        }
    }
    let count = u64::try_from(sources.len())?;
    let metadata = u64::try_from(snapshot.segments.capacity())?
        .checked_mul(size_of::<StitchSegment>() as u64)
        .and_then(|bytes| bytes.checked_add(size_of::<Vec<StitchSegment>>() as u64))
        .ok_or("条带metadata字节溢出")?;
    let result = u64::from(snapshot.width) * u64::from(snapshot.height) * 4;
    Ok(SourceAllocation {
        unique_sources: sources.len(),
        unique_capacity_bytes: capacity,
        used_pixel_bytes: used,
        effective_result_bytes: result,
        excess_capacity_bytes: capacity.saturating_sub(result),
        segment_count: snapshot.segments.len(),
        metadata_capacity_bytes: metadata,
        image_struct_bytes: count
            .checked_mul(size_of::<Image>() as u64)
            .ok_or("Image metadata字节溢出")?,
        arc_header_bytes_estimate: count
            .checked_mul((2 * size_of::<usize>()) as u64)
            .ok_or("Arc metadata字节溢出")?,
        total_process_memory: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    fn image(first_y: u32, height: u32, padding: u32) -> Image {
        let spec = Spec::physical();
        let mut bgra = Vec::with_capacity((spec.width * height * 4 + padding) as usize);
        for y in first_y..first_y + height {
            for x in 0..spec.width {
                bgra.extend_from_slice(&spec.pixel(x, y));
            }
        }
        Image {
            width: spec.width,
            height,
            bgra,
            readback_ms: 0.,
        }
    }
    fn snapshot(images: Vec<Arc<Image>>) -> StitchSnapshot {
        let mut origin = 0;
        let segments = images
            .into_iter()
            .map(|image| {
                let segment = StitchSegment {
                    source: Rect::full(image.width, image.height),
                    origin: (0, origin),
                    image,
                };
                origin += segment.source.height;
                segment
            })
            .collect();
        StitchSnapshot {
            width: 960,
            height: origin,
            axis: ScrollAxis::Vertical,
            viewport: Rect {
                x: 0,
                y: origin - 640,
                width: 960,
                height: 640,
            },
            segments,
        }
    }
    #[test]
    fn long_browser_stress_oracle_physical_budget_markers_and_world_phase_are_fixed() {
        let spec = Spec::new(1.25).unwrap();
        assert_eq!(
            u64::from(spec.width) * u64::from(spec.document_height),
            30_720_000
        );
        assert!(Spec::new(f64::NAN).is_err());
        assert!(Spec::new(1.).is_err());
        assert!(Spec::new(1.250001).is_err());
        assert_eq!(spec.pixel(0, 1000), [73, 19, 251, 255]);
        assert_eq!(spec.pixel(959, 999), [29, 181, 233, 255]);
        assert_eq!(spec.pixel(13, 1000), [61, 79, 63, 255]);
        assert_eq!(spec.pixel(500, 31000), [163, 145, 33, 255]);
        assert_eq!(spec.pixel(500, 999), [112, 148, 194, 255]);
        assert_eq!(spec.pixel(500, 1002), [26, 187, 173, 255]);
        assert_eq!(spec.pixel(500, 31999), [245, 42, 244, 255]);
        assert_ne!(
            spec.pixel(500, 999),
            spec.pixel(500, 1999),
            "噪声必须使用绝对世界行而非tile本地行"
        );
        assert_ne!(spec.pixel(500, 1002), spec.pixel(500, 2002));
        assert!(verify_image(&image(31_361, 640, 0), 31_361).is_err());
    }
    #[test]
    fn long_browser_stress_oracle_borrowed_rows_cross_tile_and_reject_gap_overlap_corrupt_tail() {
        let mut capture = StitchSnapshot {
            width: 960,
            height: 1024,
            axis: ScrollAxis::Vertical,
            viewport: Rect {
                x: 0,
                y: 384,
                width: 960,
                height: 640,
            },
            segments: vec![
                StitchSegment {
                    image: Arc::new(image(0, 640, 0)),
                    source: Rect::full(960, 640),
                    origin: (0, 0),
                },
                StitchSegment {
                    image: Arc::new(image(384, 640, 0)),
                    source: Rect {
                        x: 0,
                        y: 256,
                        width: 960,
                        height: 384,
                    },
                    origin: (0, 640),
                },
            ],
        };
        let renderer = image(384, 640, 0);
        let check = verify_snapshot(&capture, 0).unwrap();
        assert_eq!(check.pixel_check.pixels, 960 * 1024);
        assert_eq!(check.reference_bgra_bytes, 0);
        assert_eq!(
            compare_viewport(&capture, &renderer).unwrap().pixels,
            960 * 640
        );
        assert_eq!(
            verify_image(&renderer, 384).unwrap().max_channel_difference,
            0
        );
        capture.segments[1].origin.1 = 641;
        assert!(verify_snapshot(&capture, 0).is_err());
        capture.segments[1].origin.1 = 639;
        assert!(verify_snapshot(&capture, 0).is_err());
        capture.segments[1].origin.1 = 640;
        capture.segments[1].source.x = 1;
        assert!(verify_snapshot(&capture, 0).is_err());
        capture.segments[1].source.x = 0;
        let tail = Arc::get_mut(&mut capture.segments[1].image).unwrap();
        let index = tail.bgra.len() - 4;
        tail.bgra[index] ^= 127;
        assert!(verify_snapshot(&capture, 0).is_err());
        assert!(compare_viewport(&capture, &renderer).is_err());
    }
    #[test]
    fn long_browser_stress_oracle_counts_arc_identity_vec_capacity_and_dense_geometry_without_full_reference(
    ) {
        let raw = Arc::new(image(0, 640, 4096));
        let mut segments = Vec::with_capacity(32_017);
        for y in 0..32_000 {
            segments.push(StitchSegment {
                image: raw.clone(),
                source: Rect {
                    x: 0,
                    y: y % 640,
                    width: 960,
                    height: 1,
                },
                origin: (0, y),
            });
        }
        let dense = StitchSnapshot {
            width: 960,
            height: 32_000,
            axis: ScrollAxis::Vertical,
            viewport: Rect {
                x: 0,
                y: 31_360,
                width: 960,
                height: 640,
            },
            segments,
        };
        let strong = Arc::strong_count(&raw);
        let allocation = source_allocations(&dense).unwrap();
        assert_eq!(
            Arc::strong_count(&raw),
            strong,
            "统计只借用，不增加源图持有者"
        );
        assert_eq!(allocation.unique_sources, 1);
        assert_eq!(allocation.used_pixel_bytes, 960 * 640 * 4);
        assert_eq!(allocation.unique_capacity_bytes, raw.bgra.capacity() as u64);
        assert_eq!(allocation.effective_result_bytes, 122_880_000);
        assert_eq!(
            allocation.metadata_capacity_bytes,
            (size_of::<Vec<StitchSegment>>()
                + dense.segments.capacity() * size_of::<StitchSegment>()) as u64
        );
        assert!(!allocation.total_process_memory);
        let duplicate = Arc::new(image(0, 640, 8192));
        let paired = snapshot(vec![raw.clone(), duplicate.clone()]);
        let two = source_allocations(&paired).unwrap();
        assert_eq!(two.unique_sources, 2);
        assert_eq!(
            two.unique_capacity_bytes,
            (raw.bgra.capacity() + duplicate.bgra.capacity()) as u64
        );
        assert_eq!(two.used_pixel_bytes, 2 * 960 * 640 * 4);
    }
}
