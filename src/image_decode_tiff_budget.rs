//! 普通TIFF后端的可见工作区模型，不改变分页来源或压缩像素。
//! 按锁定后端的Vec布局、逐块解压和容量增长保守计费，不声称分配器或原生内部硬峰值。
use super::{
    allocate, message, preflight_bad, preflight_checkpoint, preflight_header, CheckedTag, Endian,
    JpegView, Preflight, MAX_PAGES,
};
use std::sync::atomic::AtomicBool;

const MAX_BYTES: u64 = crate::image_decode::MAX_DECODE_BYTES;
// 128层目录的借用tag数组、遍历表、首IFD BTree及小codec状态。
const DIRECTORY_BYTES: u64 = 4 * 1024 * 1024;
const STREAM_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TiffWorkspace {
    pub dimensions: (u32, u32),
    /// image适配器交付的native缓冲，不包括TIFF内部DecodingResult。
    pub native_bytes: u64,
    /// 解码器构造期间的目录、typed Value及转换数组；排来源与caller native。
    pub constructor_bytes: u64,
    /// read_image期间的内部像素、目录及最大单块codec；排来源与caller native。
    pub decode_extra_bytes: u64,
    /// image适配器set_limits要求B+最大compressed chunk；不是实际必分配字节。
    pub backend_limit_floor: u64,
}

fn budget() -> String {
    message(
        "TIFF来源与解码工作区超过256MiB预算",
        "TIFF source and decode workspace exceed the 256 MiB budget",
    )
}

fn unsupported() -> String {
    message(
        "TIFF样本或压缩模型不受当前后端支持",
        "TIFF samples or compression are unsupported by the current decoder",
    )
}

fn add(a: u64, b: u64) -> Result<u64, String> {
    a.checked_add(b).ok_or_else(budget)
}
fn mul(a: u64, b: u64) -> Result<u64, String> {
    a.checked_mul(b).ok_or_else(budget)
}
fn check(bytes: u64) -> Result<(), String> {
    if bytes > MAX_BYTES {
        Err(budget())
    } else {
        Ok(())
    }
}

/// external_source_bytes含宿主、冻结源实际capacity和其它已驻留来源数据，不能只传len。
/// 在任何预检数组分配前先留目录空间；后端构造和read_image仍须分别核调用者新增驻留。
pub(crate) fn preflight_workspace(
    bytes: &[u8],
    external_source_bytes: u64,
    cancel: &AtomicBool,
) -> Result<TiffWorkspace, String> {
    let result = (|| {
        preflight_checkpoint(Some(cancel))?;
        if external_source_bytes < bytes.len() as u64 {
            return Err(budget());
        }
        check(add(external_source_bytes, DIRECTORY_BYTES)?)?;
        let (endian, big, first) = preflight_header(bytes)?;
        let mut scan = Preflight {
            bytes,
            endian,
            big,
            seen: allocate(MAX_PAGES)?,
            external_bytes: 0,
            jpeg_metadata_bytes: 0,
            jpeg_coded_bytes: 0,
            output_budget: MAX_BYTES,
            workspace_requested: true,
            workspace: None,
            external_source_bytes,
            cancel: Some(cancel),
        };
        scan.visit(first)?;
        scan.workspace.ok_or_else(preflight_bad)
    })();
    preflight_checkpoint(Some(cancel))?;
    result
}

fn tag<'a, 'b>(entries: &'a [CheckedTag<'b>], number: u16) -> Option<&'a CheckedTag<'b>> {
    entries.iter().find(|entry| entry.tag == number)
}
fn scalar(
    entries: &[CheckedTag<'_>],
    endian: Endian,
    number: u16,
    default: u64,
) -> Result<u64, String> {
    match tag(entries, number) {
        None => Ok(default),
        Some(value) if value.count == 1 => value.unsigned(endian, 0),
        _ => Err(preflight_bad()),
    }
}
fn uniform(
    entries: &[CheckedTag<'_>],
    endian: Endian,
    number: u16,
    default: u64,
    cancel: Option<&AtomicBool>,
) -> Result<u64, String> {
    let Some(value) = tag(entries, number) else {
        return Ok(default);
    };
    if value.count == 0 {
        return Err(preflight_bad());
    }
    let first = value.unsigned(endian, 0)?;
    for index in 1..value.count {
        preflight_checkpoint(cancel)?;
        if value.unsigned(endian, index)? != first {
            return Err(unsupported());
        }
    }
    Ok(first)
}

pub(super) fn measure(
    entries: &[CheckedTag<'_>],
    endian: Endian,
    jpeg_bytes: u64,
    external: u64,
    cancel: Option<&AtomicBool>,
) -> Result<TiffWorkspace, String> {
    preflight_checkpoint(cancel)?;
    let width = scalar(entries, endian, 256, 0)?;
    let height = scalar(entries, endian, 257, 0)?;
    let pixels = mul(width, height)?;
    if width == 0
        || height == 0
        || width > 65535
        || height > 65535
        || pixels > crate::image_decode::MAX_DECODE_PIXELS
    {
        return Err(budget());
    }
    let samples = scalar(entries, endian, 277, 1)?;
    if samples == 0 || samples > u64::from(u16::MAX) {
        return Err(preflight_bad());
    }
    let bits = uniform(entries, endian, 258, 1, cancel)?;
    if tag(entries, 258).is_some_and(|value| value.count != 1 && value.count != samples) {
        return Err(preflight_bad());
    }
    let format = uniform(entries, endian, 339, 1, cancel)?;
    let extras = tag(entries, 338).map_or(0, |value| value.count);
    let photo_samples = samples.checked_sub(extras).ok_or_else(preflight_bad)?;
    let alpha = match tag(entries, 338) {
        Some(value) if value.count > 0 => matches!(value.unsigned(endian, 0)?, 1 | 2),
        _ => false,
    };
    let photo = scalar(entries, endian, 262, u64::MAX)?;
    let (out_channels, inner_channels) = match (photo, photo_samples, alpha) {
        (0 | 1, 1, _) if samples == 1 && format == 1 && matches!(bits, 1 | 8 | 16) => (1, 1),
        (2, 3 | 4, _) if (format == 1 && matches!(bits, 8 | 16)) || (format == 3 && bits == 32) => {
            let count = if photo_samples == 4 || alpha { 4 } else { 3 };
            (count, count)
        }
        (5, 4, false) if format == 1 && matches!(bits, 8 | 16) => (3, 4),
        _ => return Err(unsupported()),
    };
    let planar = scalar(entries, endian, 284, 1)?;
    if !matches!(planar, 1 | 2) {
        return Err(preflight_bad());
    }
    let native = if bits == 1 {
        pixels
    } else {
        mul(mul(pixels, out_channels)?, bits / 8)?
    };
    let inner = if bits == 1 {
        mul(width.div_ceil(8), height)?
    } else {
        mul(mul(pixels, inner_channels)?, bits / 8)?
    };
    let tiled = tag(entries, 324).is_some();
    let chunk_width = if tiled {
        scalar(entries, endian, 322, 0)?
    } else {
        width
    };
    let chunk_height = if tiled {
        scalar(entries, endian, 323, 0)?
    } else {
        scalar(entries, endian, 278, height)?
    };
    if chunk_width == 0
        || chunk_height == 0
        || chunk_width > u64::from(u32::MAX)
        || chunk_height > u64::from(u32::MAX)
    {
        return Err(preflight_bad());
    }
    let row = mul(
        mul(chunk_width, bits)?,
        if planar == 2 { 1 } else { samples },
    )?
    .div_ceil(8);
    let counts = tag(entries, if tiled { 325 } else { 279 }).ok_or_else(preflight_bad)?;
    let mut compressed = 0;
    for index in 0..counts.count {
        preflight_checkpoint(cancel)?;
        compressed = compressed.max(counts.unsigned(endian, index)?);
    }
    // Vec<Value>在64位目标的元素为32B；再计typed转换、新旧容量及持久offset/count数组。
    // 全部tag计最大count，即使该未知tag最终不被backend取值，也不低估构造预算。
    let largest_count = entries.iter().map(|value| value.count).max().unwrap_or(0);
    let tables = tag(entries, 347).map_or(0, |value| value.data.len() as u64);
    let retained = add(
        add(mul(counts.count, 16)?, tables)?,
        add(mul(samples, 8)?, mul(entries.len() as u64, 128)?)?,
    )?;
    let constructor = add(add(DIRECTORY_BYTES, retained)?, mul(largest_count, 64)?)?;
    let compression = scalar(entries, endian, 259, 1)?;
    let codec = match compression {
        // LZW仅使用decode_bytes的4096项字典及32KiB reader；Deflate为流式固定窗口。
        1 | 5 | 8 | 32946 | 32773 => STREAM_BYTES,
        // 非单调/零run也能增加transition Vec，不能只按W计费。每push消耗码流bit；
        // 两条u16行的历史容量和一次realloc以128倍压缩长度另留保守空间。
        4 => add(STREAM_BYTES, mul(compressed, 128)?)?,
        7 if jpeg_bytes > 0 => jpeg_bytes,
        _ => return Err(unsupported()),
    };
    // ExtraSamples压缩行与float predictor row-copy最多同时存在两行；Gray1还逐行expand_bits。
    let rows = add(mul(row, 2)?, if bits == 1 { mul(width, 4)? } else { 0 })?;
    let extra = add(add(inner, retained)?, add(codec, rows)?)?;
    check(add(external, constructor)?)?;
    check(add(add(external, native)?, extra)?)?;
    Ok(TiffWorkspace {
        dimensions: (width as u32, height as u32),
        native_bytes: native,
        constructor_bytes: constructor,
        decode_extra_bytes: extra,
        // Gray1适配器的total_bytes_buffer仍按展开后的P，CMYK则按4/8P。
        backend_limit_floor: add(inner.max(native), compressed)?,
    })
}

pub(super) fn jpeg_frame(
    view: &JpegView<'_>,
    at: usize,
    length: usize,
    marker: u8,
) -> Result<u64, String> {
    if !matches!(marker, 0xc0..=0xc2) || view.byte(at + 2)? != 8 {
        return Err(unsupported());
    }
    let height = u64::from(view.u16(at + 3)?);
    let width = u64::from(view.u16(at + 5)?);
    let channels = usize::from(view.byte(at + 7)?);
    if !matches!(channels, 1 | 3 | 4) || length != 8 + 3 * channels {
        return Err(preflight_bad());
    }
    let mut sampling = [(0u64, 0u64); 4];
    let mut strides = [0u64; 4];
    let mut ids = [0; 4];
    let (mut hmax, mut vmax) = (1, 1);
    for index in 0..channels {
        let id = view.byte(at + 8 + 3 * index)?;
        let factor = view.byte(at + 9 + 3 * index)?;
        let (h, v) = (u64::from(factor >> 4), u64::from(factor & 15));
        // 当前codec允许横向1/2/4，纵向1..4，包括Generic采样；不套预取JPEG子集。
        if ids[..index].contains(&id)
            || !matches!(h, 1 | 2 | 4)
            || !(1..=4).contains(&v)
            || view.byte(at + 10 + 3 * index)? > 3
        {
            return Err(unsupported());
        }
        ids[index] = id;
        sampling[index] = (h, v);
        hmax = hmax.max(h);
        vmax = vmax.max(v);
        // 后端逐组件更新max后立即冻结该组件stride；后置高采样不会重算先前组件。
        strides[index] = mul(mul(width.div_ceil(hmax * 8), h)?, 8)?;
    }
    let mx = width.div_ceil(hmax * 8);
    let my = height.div_ceil(vmax * 8);
    let mut coefficients = 0;
    let mut row_arrays = 0;
    let mut max_stride = 0;
    for (index, (h, v)) in sampling[..channels].iter().enumerate() {
        coefficients = add(coefficients, mul(mul(mul(mx, my)?, mul(*h, *v)?)?, 128)?)?;
        let stride = strides[index];
        max_stride = max_stride.max(stride);
        let ratio = mul(hmax / *h, vmax / *v)?;
        let raw = mul(stride, mul(*v, 16)?)?;
        let edges = mul(stride, mul(*v, 4)?)?;
        let first = mul(stride, mul(mul(*v, ratio)?, 2)?)?;
        let upsample = mul(stride, mul(ratio, 32)?)?;
        row_arrays = add(row_arrays, add(add(raw, edges)?, add(first, upsample)?)?)?;
    }
    // raw MCU行、上下行、first-row和upsample_dest按实际sampling算，fix因子取2；
    // 再留整批新旧行容量和最大MCU scratch，不把Generic采样降成strict子集。
    let rows = add(mul(row_arrays, 2)?, mul(max_stride, 16)?)?;
    let output = mul(mul(width, height)?, channels as u64)?;
    // read_to_end的Vec增长含新旧缓冲；只计最大当前块，不累计其它块或其它IFD像素。
    add(
        add(add(mul(view.len() as u64, 4)?, output)?, coefficients)?,
        add(rows, 4 * 1024 * 1024)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    // 只构造目录字段来检查预算，不把这些无像素码流的字段当作解码成功证明。
    fn entries(overrides: &[(u16, Vec<u32>)]) -> Vec<(u16, Vec<u8>)> {
        let mut values = vec![
            (256, vec![2]),
            (257, vec![1]),
            (258, vec![8]),
            (259, vec![1]),
            (262, vec![2]),
            (273, vec![8]),
            (277, vec![3]),
            (278, vec![1]),
            (279, vec![6]),
            (284, vec![1]),
            (339, vec![1]),
        ];
        for (tag, data) in overrides {
            if let Some(value) = values.iter_mut().find(|(number, _)| number == tag) {
                value.1 = data.clone();
            } else {
                values.push((*tag, data.clone()));
            }
        }
        values
            .into_iter()
            .map(|(tag, data)| (tag, data.into_iter().flat_map(u32::to_le_bytes).collect()))
            .collect()
    }

    fn measured(values: &[(u16, Vec<u8>)], external: u64) -> Result<TiffWorkspace, String> {
        let tags: Vec<_> = values
            .iter()
            .map(|(tag, data)| CheckedTag {
                tag: *tag,
                kind: if *tag == 347 { 7 } else { 4 },
                count: data.len() as u64 / if *tag == 347 { 1 } else { 4 },
                data,
            })
            .collect();
        measure(&tags, Endian::Little, 0, external, None)
    }

    #[test]
    fn image_tiff_workspace_cmyk16_and_extra_sample_rows_include_hidden_buffers() {
        let cmyk = measured(
            &entries(&[
                (262, vec![5]),
                (277, vec![4]),
                (258, vec![16]),
                (279, vec![16]),
            ]),
            0,
        )
        .unwrap();
        assert_eq!(cmyk.native_bytes, 12);
        assert!(cmyk.decode_extra_bytes >= 16 + 32);
        assert_eq!(cmyk.backend_limit_floor, 32);
        let rgb = measured(&entries(&[]), 0).unwrap();
        let extra = measured(
            &entries(&[(277, vec![5]), (338, vec![0, 0]), (279, vec![10])]),
            0,
        )
        .unwrap();
        assert_eq!(extra.native_bytes, rgb.native_bytes);
        assert!(extra.decode_extra_bytes >= rgb.decode_extra_bytes + 8);
        let planar = measured(&entries(&[(284, vec![2])]), 0).unwrap();
        assert!(planar.decode_extra_bytes < rgb.decode_extra_bytes);
    }

    #[test]
    fn image_tiff_workspace_oversized_tile_row_and_fax_compressed_work_are_not_canvas_only() {
        let mut tiled = entries(&[
            (322, vec![u32::MAX]),
            (323, vec![1]),
            (324, vec![8]),
            (325, vec![1]),
        ]);
        tiled.retain(|(tag, _)| !matches!(tag, 273 | 278 | 279));
        assert!(measured(&tiled, 0).is_err());
        let gray = entries(&[
            (262, vec![1]),
            (277, vec![1]),
            (258, vec![1]),
            (259, vec![4]),
            (279, vec![8]),
        ]);
        let small = measured(&gray, 0).unwrap();
        assert_eq!(small.native_bytes, 2);
        assert_eq!(small.backend_limit_floor, 10);
        let large = measured(
            &entries(&[
                (262, vec![1]),
                (277, vec![1]),
                (258, vec![1]),
                (259, vec![4]),
                (279, vec![1024]),
            ]),
            0,
        )
        .unwrap();
        assert_eq!(
            large.decode_extra_bytes - small.decode_extra_bytes,
            (1024 - 8) * 128
        );
        assert!(measured(
            &entries(&[
                (262, vec![1]),
                (277, vec![1]),
                (258, vec![1]),
                (259, vec![4]),
                (279, vec![4 * 1024 * 1024])
            ]),
            0
        )
        .is_err());
    }

    #[test]
    fn image_tiff_workspace_typed_metadata_counts_constructor_and_external_resident() {
        let mut values = entries(&[]);
        values.push((347, vec![0; 64 * 1024]));
        let plan = measured(&values, 0).unwrap();
        assert!(plan.constructor_bytes >= DIRECTORY_BYTES + 64 * (64 * 1024));
        let required = plan
            .constructor_bytes
            .max(plan.native_bytes + plan.decode_extra_bytes);
        assert!(measured(&values, MAX_BYTES - required).is_ok());
        assert!(measured(&values, MAX_BYTES - required + 1).is_err());
        for compression in [1, 5, 8, 32946, 32773] {
            assert!(measured(&entries(&[(259, vec![compression])]), 0).is_ok());
        }
    }

    fn jpeg(factors: &[u8], marker: u8) -> Vec<u8> {
        // 仅完整SOF/EOI控制头，不是可解码像素样本。
        let mut bytes = vec![0xff, 0xd8, 0xff, marker];
        bytes.extend_from_slice(&(8u16 + 3 * factors.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&[8, 0, 17, 0, 33, factors.len() as u8]);
        for (index, factor) in factors.iter().enumerate() {
            bytes.extend_from_slice(&[index as u8 + 1, *factor, 0]);
        }
        bytes.extend_from_slice(&[0xff, 0xd9]);
        bytes
    }

    #[test]
    fn image_tiff_workspace_jpeg_generic_sampling_and_split_tables_are_budgeted() {
        let early = jpeg(&[0x41, 0x11, 0x11], 0xc2);
        let late = jpeg(&[0x11, 0x41, 0x11], 0xc2);
        let mut metadata = 0;
        let early =
            super::super::preflight_jpeg_details(&[], &early, &mut metadata, MAX_BYTES, None, true)
                .unwrap();
        let mut metadata = 0;
        let late =
            super::super::preflight_jpeg_details(&[], &late, &mut metadata, MAX_BYTES, None, true)
                .unwrap();
        assert!(late > early, "后置高采样前冻结的stride更宽，须计入行工作区");
        for factors in [
            &[0x41, 0x11, 0x11][..],
            &[0x13, 0x11, 0x11],
            &[0x44, 0x11, 0x11, 0x11],
        ] {
            let bytes = jpeg(factors, 0xc2);
            let mut metadata = 0;
            let full = super::super::preflight_jpeg_details(
                &[],
                &bytes,
                &mut metadata,
                MAX_BYTES,
                None,
                true,
            )
            .unwrap();
            assert!(full > 33 * 17 * factors.len() as u64);
            let mut metadata = 0;
            let split = super::super::preflight_jpeg_details(
                &bytes[..3],
                &bytes[3..],
                &mut metadata,
                MAX_BYTES,
                None,
                true,
            )
            .unwrap();
            assert_eq!(full, split);
        }
        let bytes = jpeg(&[0x00], 0xc0);
        let mut metadata = 0;
        assert!(super::super::preflight_jpeg_details(
            &[],
            &bytes,
            &mut metadata,
            MAX_BYTES,
            None,
            true
        )
        .is_err());
    }

    #[test]
    fn image_tiff_workspace_precancel_wins_and_entropy_search_checks_cancel() {
        let cancel = AtomicBool::new(true);
        let canceled = super::super::cancelled(&cancel).unwrap_err().to_string();
        assert_eq!(preflight_workspace(&[], 0, &cancel).unwrap_err(), canceled);
        cancel.store(false, Ordering::Relaxed);
        assert!(preflight_workspace(&[], MAX_BYTES, &cancel).is_err());
        let data = vec![0u8; 128 * 1024];
        let view = JpegView {
            prefix: &[],
            data: &data,
        };
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(view.next_ff(0, Some(&cancel)).unwrap_err(), canceled);
    }
}
