//! 受控Main10静态图项直接解码为P010；不经过RGB转换、色管或视频处理器。
//! 仅核可见输入/输出缓冲；系统解码器私有堆与单次调用耗时不受本层控制。
use super::{
    coded::{self, ByteView, Cicp, CodedInfo, SampleModel},
    PlatformError, MAX_BYTES,
};
use std::{marker::PhantomData, mem::ManuallyDrop, ptr, rc::Rc, sync::atomic::AtomicBool};
use windows::{
    core::{Interface, GUID},
    Win32::{
        Foundation::{E_NOINTERFACE, E_NOTIMPL},
        Media::MediaFoundation::*,
        System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_MULTITHREADED},
    },
};

const MAX_NALS: usize = 4096;
const MAX_OUTPUT_CALLS: usize = 32;
const MAX_ACTIVATIONS: u32 = 64;
const ACTIVATION_BYTES: u64 =
    MAX_ACTIVATIONS as u64 * std::mem::size_of::<Option<IMFActivate>>() as u64;
const INPUT_ALIGNMENT_BYTES: u64 = 64 * 1024;

/// 来源已通过图项目录与实际码流检查；调用方只为无ICC且有nclx的图项构造。
#[derive(Clone, Copy)]
pub(super) struct Input<'a> {
    pub config: &'a [u8],
    pub data: ByteView<'a>,
    pub coded: CodedInfo,
    pub cicp: Cicp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Aperture {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// 原样报告MF属性枚举值；None或Unknown(0)不能当作709或其它缺省颜色。
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct OutputColor {
    pub primaries: Option<u32>,
    pub transfer: Option<u32>,
    pub matrix: Option<u32>,
    pub nominal_range: Option<u32>,
    pub chroma_siting: Option<u32>,
}

pub(super) struct RawP010 {
    /// 小端WORD，Y平面后接交错UV；低6位为零，正向紧行，不含额外方向变换。
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub aperture: Option<Aperture>,
    pub color: OutputColor,
}

#[cfg_attr(test, track_caller)]
fn subset() -> PlatformError {
    #[cfg(test)]
    eprintln!(
        "HEVC local subset rejection={}",
        std::panic::Location::caller()
    );
    coded::unsupported(
        "原生HDR路径仅支持独立HEVC Main10/4:2:0与P010输出",
        "Native HDR requires an independent HEVC Main10/4:2:0 image and P010 output",
    )
}

fn invalid() -> PlatformError {
    coded::invalid(
        "HEVC原生图片数据或输出无效",
        "Invalid native HEVC image data or output",
    )
}

#[cfg_attr(test, track_caller)]
fn call<T>(
    cancel: &AtomicBool,
    action: impl FnOnce() -> windows::core::Result<T>,
) -> Result<T, PlatformError> {
    #[cfg(test)]
    let location = std::panic::Location::caller();
    super::cancelled(cancel)?;
    let result = action();
    super::cancelled(cancel)?;
    let result = result.map_err(|error| super::native_error(error.code()));
    #[cfg(test)]
    if let Err(error) = &result {
        eprintln!("HEVC native call={location}: {error}");
    }
    result
}

fn optional<T>(
    cancel: &AtomicBool,
    action: impl FnOnce() -> windows::core::Result<T>,
) -> Result<Option<T>, PlatformError> {
    super::cancelled(cancel)?;
    let result = action();
    super::cancelled(cancel)?;
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(None),
        Err(error) => Err(super::native_error(error.code())),
    }
}

#[cfg(test)]
#[path = "image_platform_hevc_activation_diagnostic.rs"]
mod activation_diagnostic;

#[cfg(test)]
fn trace_activations(values: &[Option<IMFActivate>], cancel: &AtomicBool) {
    let mut qualified = 0usize;
    let mut metadata_errors = 0usize;
    eprintln!("HEVC activation raw_count={}", values.len());
    for (index, value) in values.iter().enumerate() {
        if super::cancelled(cancel).is_err() {
            return;
        }
        let Some(activation) = value else {
            eprintln!("HEVC activation index={index} empty=1");
            continue;
        };
        let hardware = optional(cancel, || unsafe {
            activation.GetItemType(&MFT_ENUM_HARDWARE_URL_Attribute)
        });
        let asynchronous = optional(cancel, || unsafe {
            activation.GetUINT32(&MF_TRANSFORM_ASYNC)
        });
        if super::cancelled(cancel).is_err() {
            return;
        }
        let id = unsafe { activation.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) };
        if super::cancelled(cancel).is_err() {
            return;
        }
        let eligible =
            matches!(&hardware, Ok(None)) && matches!(&asynchronous, Ok(None) | Ok(Some(0)));
        qualified += usize::from(eligible);
        metadata_errors += usize::from(hardware.is_err() || asynchronous.is_err());
        eprintln!(
            "HEVC activation index={index} hardware={hardware:?} async={asynchronous:?} clsid={:?} qualified={eligible}",
            id.as_ref().map_err(|error| error.code()),
        );
        activation_diagnostic::inspect(activation, index, cancel);
    }
    eprintln!("HEVC activation qualified_count={qualified} metadata_error_count={metadata_errors}");
}

struct Runtime {
    com: bool,
    mf: bool,
    _thread: PhantomData<Rc<()>>,
}
impl Runtime {
    fn new(cancel: &AtomicBool) -> Result<Self, PlatformError> {
        super::cancelled(cancel)?;
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok();
        let mut runtime = Self {
            com: result.is_ok(),
            mf: false,
            _thread: PhantomData,
        };
        super::cancelled(cancel)?;
        result.map_err(|error| super::native_error(error.code()))?;
        let result = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) };
        runtime.mf = result.is_ok();
        super::cancelled(cancel)?;
        result.map_err(|error| super::native_error(error.code()))?;
        Ok(runtime)
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            if self.mf {
                let _ = MFShutdown();
            }
            if self.com {
                CoUninitialize();
            }
        }
    }
}

struct Activations {
    values: *mut Option<IMFActivate>,
    count: u32,
}
impl Drop for Activations {
    fn drop(&mut self) {
        unsafe {
            if !self.values.is_null() {
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(
                    self.values,
                    self.count as usize,
                ));
                CoTaskMemFree(Some(self.values.cast()));
            }
        }
    }
}

struct Decoder {
    transform: Option<IMFTransform>,
    activation: IMFActivate,
}
impl Decoder {
    fn get(&self) -> &IMFTransform {
        self.transform.as_ref().unwrap()
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        if let Some(transform) = self.transform.take() {
            unsafe {
                let _ = transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            }
            drop(transform);
        }
        unsafe {
            let _ = self.activation.ShutdownObject();
        }
    }
}

fn merge_decoder_identity(
    previous: Option<Option<GUID>>,
    next: Option<GUID>,
) -> Result<Option<GUID>, PlatformError> {
    match previous {
        None => Ok(next),
        Some(Some(previous)) if next == Some(previous) => Ok(next),
        _ => Err(coded::unsupported(
            "无法确定唯一的P010软件解码器，暂不自动选择",
            "P010 software decoder selection is ambiguous",
        )),
    }
}

fn create_decoder(cancel: &AtomicBool) -> Result<Decoder, PlatformError> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_HEVC,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_P010,
    };
    let mut activations = Activations {
        values: ptr::null_mut(),
        count: 0,
    };
    call(cancel, || unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut activations.values,
            &mut activations.count,
        )
    })?;
    if activations.count == 0 {
        return Err(PlatformError::ComponentUnavailable(
            MF_E_TOPO_CODEC_NOT_FOUND.0,
        ));
    }
    if activations.values.is_null() || activations.count > MAX_ACTIVATIONS {
        return Err(coded::budget());
    }
    let values =
        unsafe { std::slice::from_raw_parts(activations.values, activations.count as usize) };
    #[cfg(test)]
    trace_activations(values, cancel);
    let mut selected: Option<(Option<GUID>, IMFActivate)> = None;
    for activation in values {
        super::cancelled(cancel)?;
        let activation = activation.as_ref().ok_or_else(invalid)?;
        if optional(cancel, || unsafe {
            activation.GetItemType(&MFT_ENUM_HARDWARE_URL_Attribute)
        })?
        .is_some()
        {
            continue;
        }
        if optional(cancel, || unsafe {
            activation.GetUINT32(&MF_TRANSFORM_ASYNC)
        })?
        .is_some_and(|value| value != 0)
        {
            continue;
        }
        let id = optional(cancel, || unsafe {
            activation.GetGUID(&MFT_TRANSFORM_CLSID_Attribute)
        })?;
        // 单一合格activation可直接激活；多个候选只有同一已知CLSID才能去重。
        // 缺属性只表示身份未知，不能跳过该候选再任选其它实现。
        merge_decoder_identity(selected.as_ref().map(|(id, _)| *id), id)?;
        if selected.is_none() {
            selected = Some((id, activation.clone()));
        }
    }
    let (_, activation) = selected.ok_or(PlatformError::ComponentUnavailable(
        MF_E_TOPO_CODEC_NOT_FOUND.0,
    ))?;
    // 先持有activation，Activate成功但取消时仍会Shutdown，不能遗留其内部对象。
    let mut decoder = Decoder {
        transform: None,
        activation,
    };
    super::cancelled(cancel)?;
    let result = unsafe { decoder.activation.ActivateObject::<IMFTransform>() };
    if let Ok(transform) = result.as_ref() {
        decoder.transform = Some(transform.clone());
    }
    super::cancelled(cancel)?;
    result.map_err(|error| super::native_error(error.code()))?;
    let attributes = call(cancel, || unsafe { decoder.get().GetAttributes() })?;
    let asynchronous = optional(cancel, || unsafe {
        attributes.GetUINT32(&MF_TRANSFORM_ASYNC)
    })?;
    #[cfg(test)]
    eprintln!("HEVC activated transform async={asynchronous:?}");
    if asynchronous.is_some_and(|value| value != 0) {
        return Err(subset());
    }
    Ok(decoder)
}

fn config_nals(config: &[u8]) -> Result<[(usize, usize); 3], PlatformError> {
    if config.len() < 23
        || config.len() > 64 * 1024
        || config[0] != 1
        || config[1] & 31 != 2
        || config[16] & 3 != 1
        || config[17] & 7 != 2
        || config[18] & 7 != 2
        || config[21] & 3 != 3
        || config[22] != 3
    {
        return Err(subset());
    }
    let mut ranges = [(0, 0); 3];
    let mut position = 23usize;
    for _ in 0..3 {
        let header = *config.get(position).ok_or_else(invalid)?;
        if header & 0x40 != 0 || !(32..=34).contains(&(header & 63)) {
            return Err(invalid());
        }
        let kind = usize::from((header & 63) - 32);
        let count = config.get(position + 1..position + 3).ok_or_else(invalid)?;
        if count != [0, 1] || ranges[kind].1 != 0 {
            return Err(invalid());
        }
        let length = config.get(position + 3..position + 5).ok_or_else(invalid)?;
        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
        position = position.checked_add(5).ok_or_else(coded::budget)?;
        let end = position
            .checked_add(length)
            .filter(|end| *end <= config.len())
            .ok_or_else(invalid)?;
        if length < 2 || config[position] >> 1 & 63 != header & 63 {
            return Err(invalid());
        }
        ranges[kind] = (position, length);
        position = end;
    }
    if position != config.len() || ranges.iter().any(|(_, length)| *length == 0) {
        return Err(invalid());
    }
    Ok(ranges)
}

fn walk_nals(
    input: Input<'_>,
    cancel: &AtomicBool,
    mut visit: impl FnMut(ByteView<'_>) -> Result<(), PlatformError>,
) -> Result<(), PlatformError> {
    super::cancelled(cancel)?;
    for (offset, length) in config_nals(input.config)? {
        super::cancelled(cancel)?;
        visit(ByteView::from_slice(&input.config[offset..offset + length]))?;
    }
    let mut position = 0usize;
    let mut count = 0usize;
    while position < input.data.len() {
        super::cancelled(cancel)?;
        count += 1;
        if count > MAX_NALS || input.data.len() - position < 4 {
            return Err(invalid());
        }
        let mut length = 0usize;
        for _ in 0..4 {
            length = (length << 8) | usize::from(input.data.byte(position)?);
            position += 1;
        }
        let end = position
            .checked_add(length)
            .filter(|end| *end <= input.data.len())
            .ok_or_else(invalid)?;
        if length < 2 {
            return Err(invalid());
        }
        visit(input.data.slice(position..end)?)?;
        position = end;
    }
    if count == 0 {
        return Err(invalid());
    }
    Ok(())
}

fn annex_length(input: Input<'_>, cancel: &AtomicBool) -> Result<usize, PlatformError> {
    let mut length = 0usize;
    walk_nals(input, cancel, |nal| {
        length = length
            .checked_add(4)
            .and_then(|length| length.checked_add(nal.len()))
            .filter(|length| *length as u64 <= MAX_BYTES)
            .ok_or_else(coded::budget)?;
        Ok(())
    })?;
    Ok(length)
}

fn annex_b(input: Input<'_>, length: usize, cancel: &AtomicBool) -> Result<Vec<u8>, PlatformError> {
    let mut bytes = Vec::new();
    let result = bytes.try_reserve_exact(length);
    super::cancelled(cancel)?;
    result.map_err(|_| coded::budget())?;
    if bytes.capacity() > length {
        return Err(coded::budget());
    }
    walk_nals(input, cancel, |nal| {
        if bytes
            .len()
            .checked_add(4)
            .and_then(|value| value.checked_add(nal.len()))
            .is_none_or(|end| end > length)
        {
            return Err(invalid());
        }
        bytes.extend_from_slice(&[0, 0, 0, 1]);
        let start = bytes.len();
        let end = start.checked_add(nal.len()).ok_or_else(coded::budget)?;
        while bytes.len() < end {
            super::cancelled(cancel)?;
            bytes.resize((bytes.len() + 64 * 1024).min(end), 0);
        }
        nal.copy_to(&mut bytes[start..end], cancel)?;
        Ok(())
    })?;
    if bytes.len() != length {
        return Err(invalid());
    }
    Ok(bytes)
}

fn tight_size((width, height): (u32, u32)) -> Result<usize, PlatformError> {
    if width == 0 || height == 0 || width & 1 != 0 || height & 1 != 0 {
        return Err(subset());
    }
    usize::try_from(u64::from(width) * u64::from(height) * 3).map_err(|_| coded::budget())
}

fn visible_budget(
    input: Input<'_>,
    resident: u64,
    annex_capacity: usize,
    input_capacity: u64,
    native_output: u64,
    raw: usize,
    max_output: u64,
) -> Result<(), PlatformError> {
    let pixels = coded::checked_pixels(input.coded.display_dimensions, max_output)?;
    coded::checked_pixels(input.coded.coded_dimensions, MAX_BYTES)?;
    let pictures = input.coded.picture_storage_bytes()?;
    resident
        .checked_add(ACTIVATION_BYTES)
        .and_then(|bytes| bytes.checked_add(INPUT_ALIGNMENT_BYTES))
        .and_then(|bytes| bytes.checked_add(annex_capacity as u64))
        .and_then(|bytes| bytes.checked_add(input_capacity))
        .and_then(|bytes| bytes.checked_add(pictures))
        .and_then(|bytes| bytes.checked_add(native_output))
        .and_then(|bytes| bytes.checked_add(raw as u64))
        .and_then(|bytes| bytes.checked_add(pixels * 4))
        .filter(|bytes| *bytes <= MAX_BYTES)
        .ok_or_else(coded::budget)?;
    Ok(())
}

fn expected_color(cicp: Cicp) -> Result<OutputColor, PlatformError> {
    Ok(OutputColor {
        primaries: Some(match cicp.primaries {
            1 => MFVideoPrimaries_BT709.0 as u32,
            9 => MFVideoPrimaries_BT2020.0 as u32,
            _ => return Err(subset()),
        }),
        transfer: Some(match cicp.transfer {
            16 => MFVideoTransFunc_2084.0 as u32,
            18 => MFVideoTransFunc_HLG.0 as u32,
            _ => return Err(subset()),
        }),
        matrix: Some(match cicp.matrix {
            1 => MFVideoTransferMatrix_BT709.0 as u32,
            9 => MFVideoTransferMatrix_BT2020_10.0 as u32,
            _ => return Err(subset()),
        }),
        nominal_range: Some(if cicp.full_range {
            MFNominalRange_0_255.0 as u32
        } else {
            MFNominalRange_16_235.0 as u32
        }),
        chroma_siting: None,
    })
}

fn color_attributes(
    media: &IMFMediaType,
    cancel: &AtomicBool,
) -> Result<OutputColor, PlatformError> {
    Ok(OutputColor {
        primaries: optional(cancel, || unsafe {
            media.GetUINT32(&MF_MT_VIDEO_PRIMARIES)
        })?,
        transfer: optional(cancel, || unsafe {
            media.GetUINT32(&MF_MT_TRANSFER_FUNCTION)
        })?,
        matrix: optional(cancel, || unsafe { media.GetUINT32(&MF_MT_YUV_MATRIX) })?,
        nominal_range: optional(cancel, || unsafe {
            media.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE)
        })?,
        chroma_siting: optional(cancel, || unsafe {
            media.GetUINT32(&MF_MT_VIDEO_CHROMA_SITING)
        })?,
    })
}

fn check_color(actual: OutputColor, expected: OutputColor) -> Result<(), PlatformError> {
    for (actual, expected) in [
        (actual.primaries, expected.primaries),
        (actual.transfer, expected.transfer),
        (actual.matrix, expected.matrix),
        (actual.nominal_range, expected.nominal_range),
    ] {
        if actual.is_some_and(|value| value != 0) && actual != expected {
            return Err(coded::unsupported(
                "系统P010颜色属性与来源声明不一致",
                "System P010 color attributes conflict with the source declaration",
            ));
        }
    }
    Ok(())
}

fn set_input(
    transform: &IMFTransform,
    id: u32,
    input: Input<'_>,
    expected: OutputColor,
    cancel: &AtomicBool,
) -> Result<(), PlatformError> {
    let media = call(cancel, || unsafe { MFCreateMediaType() })?;
    call(cancel, || unsafe {
        media.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
    })?;
    call(cancel, || unsafe {
        media.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)
    })?;
    call(cancel, || unsafe {
        media.SetUINT32(
            &MF_MT_MPEG2_PROFILE,
            eAVEncH265VProfile_Main_420_10.0 as u32,
        )
    })?;
    call(cancel, || unsafe {
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
    })?;
    call(cancel, || unsafe {
        media.SetUINT64(
            &MF_MT_FRAME_SIZE,
            (u64::from(input.coded.coded_dimensions.0) << 32)
                | u64::from(input.coded.coded_dimensions.1),
        )
    })?;
    call(cancel, || unsafe {
        media.SetUINT64(&MF_MT_FRAME_RATE, (1u64 << 32) | 1)
    })?;
    call(cancel, || unsafe {
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
    })?;
    for (key, value) in [
        (MF_MT_VIDEO_PRIMARIES, expected.primaries),
        (MF_MT_TRANSFER_FUNCTION, expected.transfer),
        (MF_MT_YUV_MATRIX, expected.matrix),
        (MF_MT_VIDEO_NOMINAL_RANGE, expected.nominal_range),
    ] {
        call(cancel, || unsafe { media.SetUINT32(&key, value.unwrap()) })?;
    }
    call(cancel, || unsafe { transform.SetInputType(id, &media, 0) })
}

fn aperture(
    media: &IMFMediaType,
    key: &GUID,
    dimensions: (u32, u32),
    cancel: &AtomicBool,
) -> Result<Option<Aperture>, PlatformError> {
    let Some(size) = optional(cancel, || unsafe { media.GetBlobSize(key) })? else {
        return Ok(None);
    };
    if size as usize != std::mem::size_of::<MFVideoArea>() {
        return Err(invalid());
    }
    let mut bytes = [0u8; std::mem::size_of::<MFVideoArea>()];
    let mut actual = 0;
    call(cancel, || unsafe {
        media.GetBlob(key, &mut bytes, Some(&mut actual))
    })?;
    if actual != size {
        return Err(invalid());
    }
    let area: MFVideoArea = unsafe { ptr::read_unaligned(bytes.as_ptr().cast()) };
    if area.OffsetX.fract != 0
        || area.OffsetY.fract != 0
        || area.OffsetX.value < 0
        || area.OffsetY.value < 0
        || area.Area.cx <= 0
        || area.Area.cy <= 0
    {
        return Err(subset());
    }
    let value = Aperture {
        x: area.OffsetX.value as u32,
        y: area.OffsetY.value as u32,
        width: area.Area.cx as u32,
        height: area.Area.cy as u32,
    };
    if value
        .x
        .checked_add(value.width)
        .is_none_or(|end| end > dimensions.0)
        || value
            .y
            .checked_add(value.height)
            .is_none_or(|end| end > dimensions.1)
    {
        return Err(invalid());
    }
    Ok(Some(value))
}

#[derive(Clone, Copy)]
struct OutputSpec {
    dimensions: (u32, u32),
    stride: usize,
    aperture: Option<Aperture>,
    color: OutputColor,
}

fn output_spec(
    media: &IMFMediaType,
    input: Input<'_>,
    expected: OutputColor,
    cancel: &AtomicBool,
) -> Result<OutputSpec, PlatformError> {
    if call(cancel, || unsafe { media.GetGUID(&MF_MT_MAJOR_TYPE) })? != MFMediaType_Video
        || call(cancel, || unsafe { media.GetGUID(&MF_MT_SUBTYPE) })? != MFVideoFormat_P010
    {
        return Err(subset());
    }
    let size = call(cancel, || unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE) })?;
    let dimensions = ((size >> 32) as u32, size as u32);
    #[cfg(test)]
    eprintln!(
        "HEVC output dimensions={dimensions:?} coded={:?} display={:?}",
        input.coded.coded_dimensions, input.coded.display_dimensions
    );
    if dimensions.0 == 0 || dimensions.1 == 0 {
        return Err(invalid());
    }
    coded::checked_pixels(dimensions, MAX_BYTES)?;
    if dimensions != input.coded.coded_dimensions && dimensions != input.coded.display_dimensions {
        return Err(subset());
    }
    tight_size(dimensions)?;
    let stride = optional(cancel, || unsafe { media.GetUINT32(&MF_MT_DEFAULT_STRIDE) })?
        .map_or(dimensions.0 as usize * 2, |value| value as usize);
    #[cfg(test)]
    eprintln!("HEVC output stride={stride}");
    if stride < dimensions.0 as usize * 2 || stride > i32::MAX as usize {
        return Err(subset());
    }
    if optional(cancel, || unsafe {
        media.GetUINT32(&MF_MT_PAN_SCAN_ENABLED)
    })?
    .is_some_and(|value| value != 0)
    {
        return Err(subset());
    }
    let geometric = aperture(media, &MF_MT_GEOMETRIC_APERTURE, dimensions, cancel)?;
    let minimum = aperture(media, &MF_MT_MINIMUM_DISPLAY_APERTURE, dimensions, cancel)?;
    if geometric.is_some() && minimum.is_some() && geometric != minimum {
        return Err(subset());
    }
    let color = color_attributes(media, cancel)?;
    check_color(color, expected)?;
    Ok(OutputSpec {
        dimensions,
        stride,
        aperture: geometric.or(minimum),
        color,
    })
}

fn set_output(
    transform: &IMFTransform,
    id: u32,
    input: Input<'_>,
    expected: OutputColor,
    cancel: &AtomicBool,
) -> Result<OutputSpec, PlatformError> {
    for index in 0..64 {
        super::cancelled(cancel)?;
        let result = unsafe { transform.GetOutputAvailableType(id, index) };
        super::cancelled(cancel)?;
        let media = match result {
            Ok(media) => media,
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
            Err(error) => return Err(super::native_error(error.code())),
        };
        let subtype = call(cancel, || unsafe { media.GetGUID(&MF_MT_SUBTYPE) })?;
        #[cfg(test)]
        eprintln!("HEVC output candidate index={index} subtype={subtype:?}");
        if subtype != MFVideoFormat_P010 {
            continue;
        }
        // 有些解码器先仅列出subtype；已核编码尺寸可作为输入要求，首个stream change后仍重新核实际值。
        if optional(cancel, || unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE) })?
            .is_none_or(|size| size == 0)
        {
            call(cancel, || unsafe {
                media.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(input.coded.coded_dimensions.0) << 32)
                        | u64::from(input.coded.coded_dimensions.1),
                )
            })?;
        }
        // 协商保留来源信号的YUV输出，不让候选类型中的播放缺省颜色改变目标域。
        for (key, value) in [
            (MF_MT_VIDEO_PRIMARIES, expected.primaries),
            (MF_MT_TRANSFER_FUNCTION, expected.transfer),
            (MF_MT_YUV_MATRIX, expected.matrix),
            (MF_MT_VIDEO_NOMINAL_RANGE, expected.nominal_range),
        ] {
            call(cancel, || unsafe { media.SetUINT32(&key, value.unwrap()) })?;
        }
        output_spec(&media, input, expected, cancel)?;
        call(cancel, || unsafe { transform.SetOutputType(id, &media, 0) })?;
        let current = call(cancel, || unsafe { transform.GetOutputCurrentType(id) })?;
        let checked = output_spec(&current, input, expected, cancel)?;
        return Ok(checked);
    }
    Err(subset())
}

struct BufferLock<'a> {
    buffer: &'a IMFMediaBuffer,
    data: *mut u8,
    maximum: u32,
    current: u32,
    active: bool,
}
impl<'a> BufferLock<'a> {
    fn new(buffer: &'a IMFMediaBuffer, cancel: &AtomicBool) -> Result<Self, PlatformError> {
        super::cancelled(cancel)?;
        let (mut data, mut maximum, mut current) = (ptr::null_mut(), 0, 0);
        let result = unsafe { buffer.Lock(&mut data, Some(&mut maximum), Some(&mut current)) };
        let locked = Self {
            buffer,
            data,
            maximum,
            current,
            active: result.is_ok(),
        };
        super::cancelled(cancel)?;
        result.map_err(|error| super::native_error(error.code()))?;
        if data.is_null() || current > maximum {
            return Err(invalid());
        }
        Ok(locked)
    }
    fn unlock(mut self, cancel: &AtomicBool) -> Result<(), PlatformError> {
        let result = unsafe { self.buffer.Unlock() };
        self.active = false;
        super::cancelled(cancel)?;
        result.map_err(|error| super::native_error(error.code()))
    }
}
impl Drop for BufferLock<'_> {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                let _ = self.buffer.Unlock();
            }
        }
    }
}

fn input_sample(
    bytes: &[u8],
    plan: InputBufferPlan,
    cancel: &AtomicBool,
    approve_capacity: impl FnOnce(u64) -> Result<(), PlatformError>,
) -> Result<(IMFSample, u64), PlatformError> {
    let buffer = call(cancel, || unsafe {
        MFCreateAlignedMemoryBuffer(plan.capacity, plan.mask)
    })?;
    let locked = BufferLock::new(&buffer, cancel)?;
    if locked.maximum < plan.capacity {
        return Err(invalid());
    }
    let actual_capacity = u64::from(locked.maximum);
    let approved = approve_capacity(actual_capacity);
    super::cancelled(cancel)?;
    approved?;
    for (index, chunk) in bytes.chunks(64 * 1024).enumerate() {
        super::cancelled(cancel)?;
        unsafe {
            ptr::copy_nonoverlapping(
                chunk.as_ptr(),
                locked.data.add(index * 64 * 1024),
                chunk.len(),
            );
        }
    }
    locked.unlock(cancel)?;
    // 容量满足MFT最小要求，只有实际Annex-B字节属于有效输入。
    call(cancel, || unsafe { buffer.SetCurrentLength(plan.used) })?;
    let sample = call(cancel, || unsafe { MFCreateSample() })?;
    call(cancel, || unsafe { sample.AddBuffer(&buffer) })?;
    call(cancel, || unsafe { sample.SetSampleTime(0) })?;
    call(cancel, || unsafe { sample.SetSampleDuration(10_000_000) })?;
    Ok((sample, actual_capacity))
}

struct Output {
    value: MFT_OUTPUT_DATA_BUFFER,
}
impl Drop for Output {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.value.pSample);
            ManuallyDrop::drop(&mut self.value.pEvents);
        }
    }
}

fn alignment_mask(alignment: u32) -> Result<u32, PlatformError> {
    if alignment == 0 {
        return Ok(0);
    }
    if alignment > 64 * 1024 || !alignment.is_power_of_two() {
        return Err(subset());
    }
    Ok(alignment - 1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InputBufferPlan {
    used: u32,
    capacity: u32,
    mask: u32,
}

fn input_alignment(
    info: MFT_INPUT_STREAM_INFO,
    length: usize,
) -> Result<InputBufferPlan, PlatformError> {
    let used = u32::try_from(length).map_err(|_| coded::budget())?;
    if info.dwFlags & MFT_INPUT_STREAM_FIXED_SAMPLE_SIZE.0 as u32 != 0 && info.cbSize != used
        || info.dwFlags & MFT_INPUT_STREAM_PROCESSES_IN_PLACE.0 as u32 != 0
    {
        return Err(subset());
    }
    Ok(InputBufferPlan {
        used,
        capacity: used.max(info.cbSize),
        mask: alignment_mask(info.cbAlignment)?,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Disposition {
    Sample,
    NoSample,
    FormatChange,
}

fn output_disposition(flags: u32) -> Result<Disposition, PlatformError> {
    let incomplete = MFT_OUTPUT_DATA_BUFFER_INCOMPLETE.0 as u32;
    if flags & !(incomplete | 0x300) != 0 {
        return Err(subset());
    }
    // 低位三个状态互斥，NO_SAMPLE(0x300)不能按FORMAT_CHANGE(0x100)单bit命中。
    Ok(match flags & 0x300 {
        0 => Disposition::Sample,
        0x100 => Disposition::FormatChange,
        0x200 | 0x300 => Disposition::NoSample,
        _ => unreachable!(),
    })
}

fn allocate_output(
    transform: &IMFTransform,
    id: u32,
    maximum: u64,
    cancel: &AtomicBool,
) -> Result<Output, PlatformError> {
    let info = call(cancel, || unsafe { transform.GetOutputStreamInfo(id) })?;
    let mut output = Output {
        value: MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: id,
            ..Default::default()
        },
    };
    if u64::from(info.cbSize) > maximum || info.cbAlignment > 64 * 1024 {
        return Err(coded::budget());
    }
    if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
        if info.cbSize == 0 {
            return Err(subset());
        }
        let alignment = alignment_mask(info.cbAlignment)?;
        if u64::from(info.cbSize) + u64::from(alignment) > maximum {
            return Err(coded::budget());
        }
        let buffer = call(cancel, || unsafe {
            MFCreateAlignedMemoryBuffer(info.cbSize, alignment)
        })?;
        let sample = call(cancel, || unsafe { MFCreateSample() })?;
        call(cancel, || unsafe { sample.AddBuffer(&buffer) })?;
        output.value.pSample = ManuallyDrop::new(Some(sample));
    }
    Ok(output)
}

struct Lock2D<'a> {
    buffer: &'a IMF2DBuffer2,
    active: bool,
}
impl Drop for Lock2D<'_> {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                let _ = self.buffer.Unlock2D();
            }
        }
    }
}

fn compact_copy(
    start: *const u8,
    length: usize,
    row0: usize,
    pitch: usize,
    spec: OutputSpec,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, PlatformError> {
    super::cancelled(cancel)?;
    let (width, height) = spec.dimensions;
    let size = tight_size(spec.dimensions)?;
    let row = width as usize * 2;
    let rows = height as usize + height as usize / 2;
    let required = row0
        .checked_add(pitch.checked_mul(rows - 1).ok_or_else(coded::budget)?)
        .and_then(|value| value.checked_add(row))
        .filter(|value| *value <= length)
        .ok_or_else(invalid)?;
    if start.is_null()
        || pitch < row
        || required > isize::MAX as usize
        || (start as usize).checked_add(required).is_none()
    {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    let result = bytes.try_reserve_exact(size);
    super::cancelled(cancel)?;
    result.map_err(|_| coded::budget())?;
    if bytes.capacity() > size {
        return Err(coded::budget());
    }
    for y in 0..rows {
        super::cancelled(cancel)?;
        let source = unsafe { std::slice::from_raw_parts(start.add(row0 + y * pitch), row) };
        // P010的低6位必须为0，不能把P016或其它布局误解为10位样本。
        if source.chunks_exact(2).any(|word| word[0] & 0x3f != 0) {
            return Err(invalid());
        }
        bytes.extend_from_slice(source);
    }
    super::cancelled(cancel)?;
    Ok(bytes)
}

fn readback(
    sample: &IMFSample,
    spec: OutputSpec,
    maximum: u64,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, PlatformError> {
    if call(cancel, || unsafe { sample.GetBufferCount() })? != 1 {
        return Err(subset());
    }
    let buffer = call(cancel, || unsafe { sample.GetBufferByIndex(0) })?;
    if u64::from(call(cancel, || unsafe { buffer.GetMaxLength() })?) > maximum {
        return Err(coded::budget());
    }
    super::cancelled(cancel)?;
    let two_d = buffer.cast::<IMF2DBuffer2>();
    super::cancelled(cancel)?;
    match two_d {
        Ok(two_d) => {
            let (mut row, mut pitch, mut start, mut length) =
                (ptr::null_mut(), 0, ptr::null_mut(), 0);
            super::cancelled(cancel)?;
            let result = unsafe {
                two_d.Lock2DSize(
                    MF2DBuffer_LockFlags_Read,
                    &mut row,
                    &mut pitch,
                    &mut start,
                    &mut length,
                )
            };
            let mut locked = Lock2D {
                buffer: &two_d,
                active: result.is_ok(),
            };
            super::cancelled(cancel)?;
            result.map_err(|error| super::native_error(error.code()))?;
            if length as u64 > maximum {
                return Err(coded::budget());
            }
            if pitch <= 0 {
                return Err(subset());
            }
            let offset = (row as usize)
                .checked_sub(start as usize)
                .ok_or_else(invalid)?;
            let bytes = compact_copy(start, length as usize, offset, pitch as usize, spec, cancel)?;
            let result = unsafe { two_d.Unlock2D() };
            locked.active = false;
            super::cancelled(cancel)?;
            result.map_err(|error| super::native_error(error.code()))?;
            Ok(bytes)
        }
        Err(error) if error.code() == E_NOINTERFACE => {
            // 旧2D接口不提供可访问内存范围，首批不据pitch猜跨平面边界。
            super::cancelled(cancel)?;
            let old_two_d = buffer.cast::<IMF2DBuffer>();
            super::cancelled(cancel)?;
            match old_two_d {
                Ok(_) => return Err(subset()),
                Err(error) if error.code() == E_NOINTERFACE => {}
                Err(error) => return Err(super::native_error(error.code())),
            }
            let locked = BufferLock::new(&buffer, cancel)?;
            if u64::from(locked.maximum) > maximum {
                return Err(coded::budget());
            }
            let bytes = compact_copy(
                locked.data,
                locked.current as usize,
                0,
                spec.stride,
                spec,
                cancel,
            )?;
            locked.unlock(cancel)?;
            Ok(bytes)
        }
        Err(error) => Err(super::native_error(error.code())),
    }
}

pub(super) fn decode(
    input: Input<'_>,
    resident_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<RawP010, PlatformError> {
    super::cancelled(cancel)?;
    let expected = expected_color(input.cicp)?;
    decode_with_color(input, resident_bytes, max_output_bytes, expected, cancel)
}

#[cfg(test)]
fn sdr_main10_sample_color(input: Input<'_>) -> Result<OutputColor, PlatformError> {
    // 仅为原样受控SDR样本核MFT/P010，不改变生产HDR资格或套用HDR映射。
    if input.cicp
        != (Cicp {
            primaries: 1,
            transfer: 13,
            matrix: 6,
            full_range: true,
        })
        || input.coded.coded_dimensions != (256, 128)
        || input.coded.display_dimensions != (248, 122)
        || input.coded.display_origin != (6, 4)
    {
        return Err(subset());
    }
    Ok(OutputColor {
        primaries: Some(MFVideoPrimaries_BT709.0 as u32),
        transfer: Some(MFVideoTransFunc_sRGB.0 as u32),
        matrix: Some(MFVideoTransferMatrix_BT601.0 as u32),
        nominal_range: Some(MFNominalRange_0_255.0 as u32),
        chroma_siting: None,
    })
}

#[cfg(test)]
fn decode_sdr_main10_sample(
    input: Input<'_>,
    resident_bytes: u64,
    max_output_bytes: u64,
    cancel: &AtomicBool,
) -> Result<RawP010, PlatformError> {
    super::cancelled(cancel)?;
    let expected = sdr_main10_sample_color(input)?;
    decode_with_color(input, resident_bytes, max_output_bytes, expected, cancel)
}

fn decode_with_color(
    input: Input<'_>,
    resident_bytes: u64,
    max_output_bytes: u64,
    expected: OutputColor,
    cancel: &AtomicBool,
) -> Result<RawP010, PlatformError> {
    super::cancelled(cancel)?;
    let (width, height) = input.coded.coded_dimensions;
    if input.coded.bits_per_channel != 10
        || input.coded.color.model != SampleModel::Ycc
        || width < 48
        || height < 48
        || width > 4096
        || height > 2304
        || input.coded.color.cicp != Some(input.cicp)
    {
        return Err(subset());
    }
    let raw_max = tight_size(input.coded.coded_dimensions)?;
    // 包含输出样本的可见对齐余量；解码器私有picture存储另按码流模型计入。
    let native_max = u64::from(width) * u64::from(height) * 8 + 64 * 1024;
    let length = annex_length(input, cancel)?;
    visible_budget(
        input,
        resident_bytes,
        length,
        length as u64,
        native_max,
        raw_max,
        max_output_bytes,
    )?;
    let annex = annex_b(input, length, cancel)?;
    visible_budget(
        input,
        resident_bytes,
        annex.capacity(),
        annex.len() as u64,
        native_max,
        raw_max,
        max_output_bytes,
    )?;
    let _runtime = Runtime::new(cancel)?;
    let decoder = create_decoder(cancel)?;
    let transform = decoder.get();
    let (mut inputs, mut outputs) = (0, 0);
    call(cancel, || unsafe {
        transform.GetStreamCount(&mut inputs, &mut outputs)
    })?;
    #[cfg(test)]
    eprintln!("HEVC activated streams input={inputs} output={outputs}");
    if (inputs, outputs) != (1, 1) {
        return Err(subset());
    }
    let (mut input_ids, mut output_ids) = ([0], [0]);
    super::cancelled(cancel)?;
    let result = unsafe { transform.GetStreamIDs(&mut input_ids, &mut output_ids) };
    super::cancelled(cancel)?;
    if let Err(error) = result {
        if error.code() != E_NOTIMPL {
            return Err(super::native_error(error.code()));
        }
        input_ids[0] = 0;
        output_ids[0] = 0;
    }
    let (input_id, output_id) = (input_ids[0], output_ids[0]);
    set_input(transform, input_id, input, expected, cancel)?;
    let mut spec = set_output(transform, output_id, input, expected, cancel)?;
    let mut input_info = MFT_INPUT_STREAM_INFO::default();
    call(cancel, || unsafe {
        transform.GetInputStreamInfo(input_id, &mut input_info)
    })?;
    #[cfg(test)]
    eprintln!(
        "HEVC input stream flags=0x{:08x} minimum_bytes={} alignment={} max_lookahead={} latency={} annex_bytes={}",
        input_info.dwFlags,
        input_info.cbSize,
        input_info.cbAlignment,
        input_info.cbMaxLookahead,
        input_info.hnsMaxLatency,
        annex.len()
    );
    let input_plan = input_alignment(input_info, annex.len())?;
    visible_budget(
        input,
        resident_bytes,
        annex.capacity(),
        u64::from(input_plan.capacity),
        native_max,
        raw_max,
        max_output_bytes,
    )?;
    let (sample, input_capacity) = input_sample(&annex, input_plan, cancel, |actual_capacity| {
        visible_budget(
            input,
            resident_bytes,
            annex.capacity(),
            actual_capacity,
            native_max,
            raw_max,
            max_output_bytes,
        )
    })?;
    call(cancel, || unsafe {
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
    })?;
    call(cancel, || unsafe {
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
    })?;
    call(cancel, || unsafe {
        transform.ProcessInput(input_id, &sample, 0)
    })?;
    // MFT可以保留输入sample；其实际容量持续计入后续输出与读回峰值。
    drop(sample);
    call(cancel, || unsafe {
        transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, input_id as usize)
    })?;
    call(cancel, || unsafe {
        transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
    })?;
    let mut decoded = None;
    let mut changes = 0;
    for _ in 0..MAX_OUTPUT_CALLS {
        super::cancelled(cancel)?;
        let mut output = allocate_output(transform, output_id, native_max, cancel)?;
        let mut status = 0;
        let result = unsafe {
            transform.ProcessOutput(0, std::slice::from_mut(&mut output.value), &mut status)
        };
        super::cancelled(cancel)?;
        match result {
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                if decoded.is_some() || changes >= 4 {
                    return Err(invalid());
                }
                changes += 1;
                drop(output);
                spec = set_output(transform, output_id, input, expected, cancel)?;
            }
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                let bytes = decoded.ok_or_else(invalid)?;
                super::cancelled(cancel)?;
                return Ok(RawP010 {
                    bytes,
                    width: spec.dimensions.0,
                    height: spec.dimensions.1,
                    stride: spec.dimensions.0 as usize * 2,
                    aperture: spec.aperture,
                    color: spec.color,
                });
            }
            Err(error) => return Err(super::native_error(error.code())),
            Ok(()) => {
                if status != 0 {
                    return Err(subset());
                }
                match output_disposition(output.value.dwStatus)? {
                    Disposition::NoSample => continue,
                    Disposition::FormatChange => {
                        if decoded.is_some() || changes >= 4 {
                            return Err(invalid());
                        }
                        changes += 1;
                        drop(output);
                        spec = set_output(transform, output_id, input, expected, cancel)?;
                        continue;
                    }
                    Disposition::Sample => {}
                }
                let Some(sample) = output.value.pSample.as_ref() else {
                    continue;
                };
                if decoded.is_some() {
                    return Err(invalid());
                }
                let current = call(cancel, || unsafe {
                    transform.GetOutputCurrentType(output_id)
                })?;
                spec = output_spec(&current, input, expected, cancel)?;
                visible_budget(
                    input,
                    resident_bytes,
                    annex.capacity(),
                    input_capacity,
                    native_max,
                    tight_size(spec.dimensions)?,
                    max_output_bytes,
                )?;
                let bytes = readback(sample, spec, native_max, cancel)?;
                visible_budget(
                    input,
                    resident_bytes,
                    annex.capacity(),
                    input_capacity,
                    native_max,
                    bytes.capacity(),
                    max_output_bytes,
                )?;
                decoded = Some(bytes);
            }
        }
    }
    Err(coded::unsupported(
        "系统HEVC解码未在有界调用内完成单帧",
        "System HEVC decoding did not finish one frame within the bounded call count",
    ))
}

#[cfg(test)]
#[path = "image_platform_hevc_main10_native_tests.rs"]
mod main10_native_tests;

#[cfg(test)]
mod tests {
    use super::super::coded::{CodedColor, SampleModel};
    use super::*;

    #[test]
    fn image_platform_hevc_native_candidate_identity_requires_unique_or_known_duplicates() {
        let first = GUID::from_u128(1);
        let second = GUID::from_u128(2);
        let choose = |ids: &[Option<GUID>]| {
            let mut selected = None;
            for id in ids {
                selected = Some(merge_decoder_identity(selected, *id)?);
            }
            selected.ok_or_else(subset)
        };
        assert_eq!(choose(&[None]).unwrap(), None);
        assert_eq!(choose(&[Some(first)]).unwrap(), Some(first));
        assert_eq!(choose(&[Some(first), Some(first)]).unwrap(), Some(first));
        for ambiguous in [
            [None, None],
            [None, Some(first)],
            [Some(first), None],
            [Some(first), Some(second)],
        ] {
            assert!(matches!(
                choose(&ambiguous),
                Err(PlatformError::Unsupported { .. })
            ));
        }
    }

    fn config() -> Vec<u8> {
        let mut bytes = vec![0; 23];
        bytes[0] = 1;
        bytes[1] = 2;
        bytes[16] = 1;
        bytes[17] = 2;
        bytes[18] = 2;
        bytes[21] = 3;
        bytes[22] = 3;
        // 故意按PPS/VPS/SPS目录顺序，重封必须仍是VPS/SPS/PPS。
        for kind in [34, 32, 33] {
            bytes.extend_from_slice(&[0x80 | kind, 0, 1, 0, 3, kind << 1, 1, kind]);
        }
        bytes
    }

    fn make_input<'a>(config: &'a [u8], data: &'a [u8]) -> Input<'a> {
        let cicp = Cicp {
            primaries: 9,
            transfer: 16,
            matrix: 9,
            full_range: false,
        };
        Input {
            config,
            data: ByteView::from_slice(data),
            cicp,
            coded: CodedInfo {
                coded_dimensions: (48, 48),
                display_dimensions: (48, 48),
                display_origin: (0, 0),
                bits_per_channel: 10,
                picture_buffers: 1,
                color: CodedColor {
                    model: SampleModel::Ycc,
                    cicp: Some(cicp),
                },
            },
        }
    }

    #[test]
    fn image_platform_hevc_native_annex_b_uses_parameter_order_and_logical_nals() {
        let config = config();
        let data = [0, 0, 0, 3, 38, 1, 0xa5, 0, 0, 0, 2, 80, 1];
        let cancel = AtomicBool::new(false);
        let input = make_input(&config, &data);
        let length = annex_length(input, &cancel).unwrap();
        let actual = annex_b(input, length, &cancel).unwrap();
        let expected = [
            0, 0, 0, 1, 64, 1, 32, 0, 0, 0, 1, 66, 1, 33, 0, 0, 0, 1, 68, 1, 34, 0, 0, 0, 1, 38, 1,
            0xa5, 0, 0, 0, 1, 80, 1,
        ];
        assert_eq!(actual, expected);
        assert_eq!(length, expected.len());
        assert!(annex_length(make_input(&config, &data[..data.len() - 1]), &cancel).is_err());
        assert!(matches!(
            annex_length(input, &AtomicBool::new(true)),
            Err(PlatformError::Cancelled)
        ));
    }

    #[test]
    fn image_platform_hevc_native_budget_separates_output_and_visible_capacity() {
        let config = config();
        let data = [0, 0, 0, 2, 38, 1];
        let input = make_input(&config, &data);
        let raw = 48 * 48 * 3;
        let native = 48 * 48 * 8 + 64 * 1024;
        let pixels = 48 * 48 * 4;
        let other = ACTIVATION_BYTES
            + INPUT_ALIGNMENT_BYTES
            + 20
            + input.coded.picture_storage_bytes().unwrap()
            + native
            + raw as u64
            + pixels;
        assert!(visible_budget(input, MAX_BYTES - other, 10, 10, native, raw, pixels).is_ok());
        assert!(matches!(
            visible_budget(input, MAX_BYTES - other + 1, 10, 10, native, raw, pixels),
            Err(PlatformError::Budget(_))
        ));
        assert!(matches!(
            visible_budget(input, 0, 10, 10, native, raw, pixels - 1),
            Err(PlatformError::Budget(_))
        ));
        assert!(matches!(
            visible_budget(input, u64::MAX, usize::MAX, 10, native, raw, pixels),
            Err(PlatformError::Budget(_))
        ));
    }

    #[test]
    fn image_platform_hevc_native_alignment_and_output_status_are_not_bit_aliases() {
        assert_eq!(alignment_mask(0).unwrap(), 0);
        assert_eq!(alignment_mask(1).unwrap(), 0);
        assert_eq!(alignment_mask(16).unwrap(), 15);
        assert!(alignment_mask(15).is_err());
        let mut input = MFT_INPUT_STREAM_INFO {
            cbSize: 10,
            cbAlignment: 16,
            ..Default::default()
        };
        assert_eq!(
            input_alignment(input, 10).unwrap(),
            InputBufferPlan {
                used: 10,
                capacity: 10,
                mask: 15,
            }
        );
        input.cbSize = 11;
        assert_eq!(
            input_alignment(input, 10).unwrap(),
            InputBufferPlan {
                used: 10,
                capacity: 11,
                mask: 15,
            }
        );
        input.cbSize = 9;
        input.dwFlags = MFT_INPUT_STREAM_FIXED_SAMPLE_SIZE.0 as u32;
        assert!(input_alignment(input, 10).is_err());
        input.cbSize = 10;
        assert_eq!(input_alignment(input, 10).unwrap().used, 10);
        input.dwFlags = MFT_INPUT_STREAM_PROCESSES_IN_PLACE.0 as u32;
        assert!(input_alignment(input, 10).is_err());
        assert_eq!(output_disposition(0).unwrap(), Disposition::Sample);
        assert_eq!(
            output_disposition(0x100).unwrap(),
            Disposition::FormatChange
        );
        assert_eq!(output_disposition(0x200).unwrap(), Disposition::NoSample);
        assert_eq!(output_disposition(0x300).unwrap(), Disposition::NoSample);
        assert_eq!(
            output_disposition(0x0100_0000).unwrap(),
            Disposition::Sample
        );
        assert!(output_disposition(1).is_err());
    }

    #[test]
    fn image_platform_hevc_native_variable_input_minimum_has_an_independent_capacity_budget() {
        let config = config();
        let data = [0, 0, 0, 2, 38, 1];
        let input = make_input(&config, &data);
        let plan = input_alignment(
            MFT_INPUT_STREAM_INFO {
                dwFlags: 0x3,
                cbSize: 49_152,
                ..Default::default()
            },
            2_313,
        )
        .unwrap();
        assert_eq!(
            plan,
            InputBufferPlan {
                used: 2_313,
                capacity: 49_152,
                mask: 0,
            }
        );
        let raw = 48 * 48 * 3;
        let native = 48 * 48 * 8 + 64 * 1024;
        let pixels = 48 * 48 * 4;
        let other = ACTIVATION_BYTES
            + INPUT_ALIGNMENT_BYTES
            + u64::from(plan.used)
            + u64::from(plan.capacity)
            + input.coded.picture_storage_bytes().unwrap()
            + native
            + raw as u64
            + pixels;
        let resident = MAX_BYTES - other;
        let check = |capacity| {
            visible_budget(
                input,
                resident,
                plan.used as usize,
                capacity,
                native,
                raw,
                pixels,
            )
        };
        assert!(check(u64::from(plan.capacity)).is_ok());
        assert!(matches!(
            check(u64::from(plan.capacity) + 1),
            Err(PlatformError::Budget(_))
        ));
        assert!(matches!(check(u64::MAX), Err(PlatformError::Budget(_))));
    }

    #[test]
    fn image_platform_hevc_native_p010_compact_copy_keeps_planes_and_rejects_bad_words() {
        let mut padded = [0xcc; 24];
        let words = [64u16, 128, 192, 256, 320, 384];
        for (row, values) in words.chunks(2).enumerate() {
            for (x, word) in values.iter().enumerate() {
                padded[row * 8 + x * 2..row * 8 + x * 2 + 2].copy_from_slice(&word.to_le_bytes());
            }
        }
        let spec = OutputSpec {
            dimensions: (2, 2),
            stride: 8,
            aperture: None,
            color: OutputColor::default(),
        };
        let cancel = AtomicBool::new(false);
        let actual = compact_copy(padded.as_ptr(), padded.len(), 0, 8, spec, &cancel).unwrap();
        let expected: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        assert_eq!(actual, expected);
        assert!(compact_copy(padded.as_ptr(), 19, 0, 8, spec, &cancel).is_err());
        padded[16] |= 1;
        assert!(matches!(
            compact_copy(padded.as_ptr(), padded.len(), 0, 8, spec, &cancel),
            Err(PlatformError::Invalid { .. })
        ));
        assert!(matches!(
            compact_copy(
                padded.as_ptr(),
                padded.len(),
                0,
                8,
                spec,
                &AtomicBool::new(true)
            ),
            Err(PlatformError::Cancelled)
        ));
    }

    #[test]
    fn image_platform_hevc_native_color_negotiation_keeps_missing_and_rejects_conflicts() {
        let expected = expected_color(Cicp {
            primaries: 9,
            transfer: 18,
            matrix: 9,
            full_range: true,
        })
        .unwrap();
        assert!(check_color(OutputColor::default(), expected).is_ok());
        assert!(check_color(
            OutputColor {
                primaries: Some(0),
                transfer: Some(0),
                matrix: Some(0),
                nominal_range: Some(0),
                chroma_siting: Some(0)
            },
            expected
        )
        .is_ok());
        assert!(check_color(expected, expected).is_ok());
        assert!(check_color(
            OutputColor {
                transfer: Some(MFVideoTransFunc_709.0 as u32),
                ..expected
            },
            expected
        )
        .is_err());
        assert!(tight_size((49, 48)).is_err());
        assert!(tight_size((48, 49)).is_err());
    }
}
