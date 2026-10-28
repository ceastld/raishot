//! 常规尺寸AVIF的原始／冻结输入对照；不替代白色1×1样本，也不提供绝对RGB金值。
use super::*;

const SOURCE: &[u8] = include_bytes!("../tests/data/image_platform/kodim03_yuv420_8bpc.avif");
const DIMENSIONS: (u32, u32) = (768, 512);
const INPUT_LIMIT: usize = 64 * 1024;
const OUTPUT_LIMIT: u64 = 2 * 1024 * 1024;

fn be32(bytes: &[u8], offset: usize) -> usize {
    u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize
}

fn assert_boxes(bytes: &[u8], range: std::ops::Range<usize>, expected: &[([u8; 4], usize)]) {
    let mut offset = range.start;
    for (kind, length) in expected {
        assert_eq!(be32(bytes, offset), *length);
        assert_eq!(&bytes[offset + 4..offset + 8], kind);
        offset += length;
    }
    assert_eq!(offset, range.end);
}

#[test]
fn image_platform_avif_baseline_preflight_facts_and_frozen_roundtrip() {
    assert_eq!(SOURCE.len(), 25_430);
    assert!(SOURCE.len() <= INPUT_LIMIT);
    assert_eq!(detect(SOURCE), Ok(Some(PlatformFormat::Avif)));
    // 完整顶层和meta子目录没有轨道或额外关系；唯一infe是可见av01主图。
    assert_boxes(
        SOURCE,
        0..SOURCE.len(),
        &[(*b"ftyp", 32), (*b"meta", 242), (*b"mdat", 25_156)],
    );
    assert_boxes(
        SOURCE,
        44..274,
        &[
            (*b"hdlr", 40),
            (*b"pitm", 14),
            (*b"iloc", 30),
            (*b"iinf", 40),
            (*b"iprp", 106),
        ],
    );
    assert_eq!(&SOURCE[96..98], &[0, 1]);
    assert_eq!(&SOURCE[136..142], &[0, 0, 0, 0, 0, 1]);
    assert_boxes(SOURCE, 142..168, &[(*b"infe", 26)]);
    assert_eq!(
        &SOURCE[150..162],
        &[2, 0, 0, 0, 0, 1, 0, 0, b'a', b'v', b'0', b'1'],
    );
    assert_boxes(
        SOURCE,
        184..251,
        &[
            (*b"ispe", 20),
            (*b"pixi", 16),
            (*b"av1C", 12),
            (*b"colr", 19),
        ],
    );
    assert_eq!((be32(SOURCE, 196), be32(SOURCE, 200)), (768, 512));
    assert_eq!(&SOURCE[216..220], &[3, 8, 8, 8]);
    // profile0、8位、非单色、subsampling_x/y均为1；预检还须绑定实际Sequence Header。
    assert_eq!(&SOURCE[228..232], &[0x81, 0x04, 0x0c, 0]);

    let cancel = AtomicBool::new(false);
    let plan = preflight::inspect(SOURCE, OUTPUT_LIMIT, &cancel).unwrap();
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.catalog().len(), 1);
    assert_eq!(plan.primary_index(), 0);
    let item = plan.catalog()[0];
    assert_eq!(item.item_id, 1);
    assert!(!item.hidden);
    assert_eq!(item.orientation, None);
    assert_eq!(item.coded.coded_dimensions, DIMENSIONS);
    assert_eq!(item.coded.display_dimensions, DIMENSIONS);
    assert_eq!(item.coded.display_origin, (0, 0));
    assert_eq!(item.coded.bits_per_channel, 8);
    assert_eq!(item.coded.picture_buffers, 1);
    assert_eq!(item.color.coded_color.model, SampleModel::Ycc);
    assert!(plan.icc_profile(0).unwrap().is_none());
    let frozen = plan.freeze_item(0, &cancel).unwrap();
    assert_eq!(frozen.bytes.len(), plan.frozen_bytes(0).unwrap());
    assert!(frozen.bytes.len() <= INPUT_LIMIT);
    let checked = preflight::inspect(&frozen.bytes, OUTPUT_LIMIT, &cancel).unwrap();
    assert_eq!(checked.items.len(), 1);
    assert_eq!(checked.catalog().len(), 1);
    assert_eq!(checked.primary_index(), 0);
    assert_eq!(checked.catalog()[0].item_id, 1);
    assert_eq!(checked.catalog()[0].coded, item.coded);
    assert_eq!(checked.catalog()[0].orientation, None);
    assert_eq!(
        checked.catalog()[0].color.container_cicp,
        item.color.container_cicp,
    );
    assert!(checked.icc_profile(0).unwrap().is_none());
}

struct BaselinePixels {
    image: Image,
    native_format: BitmapPixelFormat,
    native_alpha: BitmapAlphaMode,
}

impl fmt::Debug for BaselinePixels {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let first = self.image.bgra.get(..3).unwrap_or(&[]);
        let nonuniform = self
            .image
            .bgra
            .chunks_exact(4)
            .any(|pixel| &pixel[..3] != first);
        formatter
            .debug_struct("BaselinePixels")
            .field("dimensions", &(self.image.width, self.image.height))
            .field("native_format", &self.native_format)
            .field("native_alpha", &self.native_alpha)
            .field("bytes", &self.image.bgra.len())
            .field(
                "opaque",
                &self.image.bgra.chunks_exact(4).all(|pixel| pixel[3] == 255),
            )
            .field("nonuniform_rgb", &nonuniform)
            .finish()
    }
}

fn baseline_pixels(bytes: &[u8], cancel: &AtomicBool) -> Result<BaselinePixels, PlatformError> {
    cancelled(cancel)?;
    let expected = pixels(DIMENSIONS)?.checked_mul(4).ok_or_else(budget)?;
    if bytes.len() > INPUT_LIMIT || expected > OUTPUT_LIMIT {
        return Err(budget());
    }
    let _apartment = Apartment::initialize(cancel)?;
    let memory = call(cancel, || {
        unsafe { SHCreateMemStream(Some(bytes)) }.ok_or(PlatformError::Native(E_OUTOFMEMORY.0))
    })?;
    let stream: IRandomAccessStream = native_at("baseline stream", cancel, || unsafe {
        CreateRandomAccessStreamOverStream(&memory, BSOS_DEFAULT)
    })?;
    let operation = native_at("baseline decoder create", cancel, || {
        BitmapDecoder::CreateAsync(&stream)
    })?;
    let decoder = native_at("baseline decoder completion", cancel, || operation.get())?;
    drop(operation);
    if native_at("baseline frame count", cancel, || decoder.FrameCount())? != 1 {
        return Err(invalid(
            "AVIF基线样本必须只有一帧",
            "The AVIF baseline sample must have one frame",
        ));
    }
    let operation = native_at("baseline frame open", cancel, || decoder.GetFrameAsync(0))?;
    let frame = native_at("baseline frame completion", cancel, || operation.get())?;
    drop(operation);
    let raw = (
        native_at("baseline width", cancel, || frame.PixelWidth())?,
        native_at("baseline height", cancel, || frame.PixelHeight())?,
    );
    let oriented = (
        native_at("baseline oriented width", cancel, || {
            frame.OrientedPixelWidth()
        })?,
        native_at("baseline oriented height", cancel, || {
            frame.OrientedPixelHeight()
        })?,
    );
    // 在请求任何像素前核实际尺寸；不能用缩放参数把未知大图变成安全小图。
    if raw != DIMENSIONS || oriented != DIMENSIONS {
        return Err(invalid(
            "AVIF基线样本的系统尺寸与已核原尺寸不一致",
            "System dimensions differ from the checked AVIF baseline dimensions",
        ));
    }
    let native_format = native_at("baseline native format", cancel, || {
        frame.BitmapPixelFormat()
    })?;
    let native_alpha = native_at("baseline native alpha", cancel, || frame.BitmapAlphaMode())?;
    eprintln!("AVIF baseline dimensions={raw:?} native={native_format:?}/{native_alpha:?}");
    let output = output_budget(
        bytes.len(),
        bytes.len() as u64,
        raw,
        oriented,
        native_format,
        OUTPUT_LIMIT,
    )?;
    let transform = native_at("baseline transform", cancel, BitmapTransform::new)?;
    let operation = native_at("baseline pixel request", cancel, || {
        frame.GetPixelDataTransformedAsync(
            BitmapPixelFormat::Bgra8,
            BitmapAlphaMode::Straight,
            &transform,
            ExifOrientationMode::IgnoreExifOrientation,
            ColorManagementMode::DoNotColorManage,
        )
    })?;
    let provider = native_at("baseline pixel completion", cancel, || operation.get())?;
    drop(operation);
    let data = native_at("baseline pixel detach", cancel, || {
        provider.DetachPixelData()
    })?;
    drop(provider);
    drop(transform);
    drop(frame);
    drop(decoder);
    drop(stream);
    drop(memory);
    let image = copy_pixels(&data, raw, output, Orientation::NoTransforms, cancel)?;
    Ok(BaselinePixels {
        image,
        native_format,
        native_alpha,
    })
}

#[test]
#[ignore = "常规8位420 AVIF原始／冻结系统像素对照；需显式运行，无窗口"]
fn image_platform_native_avif_baseline_original_and_frozen_pixels() {
    let cancel = AtomicBool::new(false);
    let plan = preflight::inspect(SOURCE, OUTPUT_LIMIT, &cancel).unwrap();
    assert_eq!(plan.catalog().len(), 1);
    assert_eq!(plan.catalog()[0].coded.display_dimensions, DIMENSIONS);
    let frozen = plan.freeze_item(0, &cancel).unwrap();
    assert!(SOURCE.len() <= INPUT_LIMIT && frozen.bytes.len() <= INPUT_LIMIT);

    let original = baseline_pixels(SOURCE, &cancel);
    eprintln!(
        "AVIF baseline view=original input_bytes={} result={original:?}",
        SOURCE.len()
    );
    let rebuilt = baseline_pixels(&frozen.bytes, &cancel);
    eprintln!(
        "AVIF baseline view=frozen input_bytes={} result={rebuilt:?}",
        frozen.bytes.len()
    );
    // 两份结果均已记录后才失败；任何HRESULT都不能被转成条件跳过或像素通过。
    let original = original.unwrap_or_else(|error| panic!("AVIF baseline original: {error}"));
    let rebuilt = rebuilt.unwrap_or_else(|error| panic!("AVIF baseline frozen: {error}"));
    for pixels in [&original, &rebuilt] {
        assert_eq!((pixels.image.width, pixels.image.height), DIMENSIONS);
        assert_eq!(pixels.image.bgra.len(), 768 * 512 * 4);
        assert!(pixels
            .image
            .bgra
            .chunks_exact(4)
            .all(|pixel| pixel[3] == 255));
        assert!(pixels
            .image
            .bgra
            .chunks_exact(4)
            .any(|pixel| pixel[..3] != pixels.image.bgra[..3]));
    }
    let mismatch = original
        .image
        .bgra
        .chunks_exact(4)
        .zip(rebuilt.image.bgra.chunks_exact(4))
        .enumerate()
        .find(|(_, (original, rebuilt))| original != rebuilt);
    assert!(
        mismatch.is_none(),
        "AVIF baseline first differing pixel: {mismatch:?}"
    );
}
