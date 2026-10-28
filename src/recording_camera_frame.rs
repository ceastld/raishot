//! 内部摄像头试验的 RGB32 帧整理与物理像素画中画；不枚举、打开或控制设备。
//! 只接纳当前类型的整数 aperture、正方形像素和零旋转，不推测缺损行或动态格式。
//! 合成仅做保持比例的最近邻缩放，不覆盖屏幕全局坐标、标注状态或录制时钟。
use crate::{
    capture::Image,
    geometry::{Rect, ScreenRect},
    recording_size_layout::RecordingSizeLayout,
    Result,
};
use std::{mem::size_of, ptr};
use windows::{
    core::{Interface, GUID},
    Win32::{Foundation::E_NOINTERFACE, Media::MediaFoundation::*},
};

const MAX_CODED_SIDE: u32 = 4096;
const MAX_PIXELS: u64 = 4_194_304;
const MAX_DECODED_BYTES: usize = 16 * 1024 * 1024;
const MAX_BUFFER_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CameraFrame {
    pub width: u32,
    pub height: u32,
    /// 紧密排列的 top-down BGRA，整理后的 alpha 固定为 255。
    pub bgra: Vec<u8>,
}

/// 只保存数值，不持有 COM 对象；owner 可重新查询 current type 后严格比较。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CameraLayout {
    coded: (u32, u32),
    visible: Rect,
    default_stride: i32,
    frame_rate: Option<(u32, u32)>,
    pixel_aspect_ratio: (u32, u32),
    interlace_mode: Option<u32>,
    pan_scan_enabled: bool,
    // 即使未选中的 aperture 发生变化也不能冒充同一 current 布局。
    apertures: [Option<Rect>; 3],
}

impl CameraLayout {
    pub(crate) fn from_current_type(media: &IMFMediaType) -> Result<Self> {
        if unsafe { media.GetGUID(&MF_MT_MAJOR_TYPE)? } != MFMediaType_Video
            || unsafe { media.GetGUID(&MF_MT_SUBTYPE)? } != MFVideoFormat_RGB32
        {
            return Err("摄像头帧只接受当前 Video/RGB32 类型".into());
        }
        let size = unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE)? };
        let coded = ((size >> 32) as u32, size as u32);
        pixel_bytes(coded.0, coded.1)?;
        let default_stride = match optional_u32(media, &MF_MT_DEFAULT_STRIDE)? {
            Some(value) => value as i32,
            None => unsafe { MFGetStrideForBitmapInfoHeader(MFVideoFormat_RGB32.data1, coded.0)? },
        };
        let mut layout = Self::new(coded, None, default_stride)?;
        require_zero_rotation(optional_u32(media, &MF_MT_VIDEO_ROTATION)?)?;
        layout.pixel_aspect_ratio =
            square_pixel_ratio(optional_u64(media, &MF_MT_PIXEL_ASPECT_RATIO)?)?;
        layout.frame_rate = optional_u64(media, &MF_MT_FRAME_RATE)?
            .map(valid_ratio)
            .transpose()?;
        layout.interlace_mode = optional_u32(media, &MF_MT_INTERLACE_MODE)?;
        if layout
            .interlace_mode
            .is_some_and(|mode| mode != MFVideoInterlace_Progressive.0 as u32)
        {
            return Err("摄像头帧暂不接纳非逐行扫描类型".into());
        }
        layout.pan_scan_enabled = match optional_u32(media, &MF_MT_PAN_SCAN_ENABLED)? {
            None | Some(0) => false,
            Some(1) => true,
            Some(_) => return Err("摄像头 pan-scan 标志无效".into()),
        };
        layout.apertures = [
            optional_aperture(media, &MF_MT_PAN_SCAN_APERTURE, coded)?,
            optional_aperture(media, &MF_MT_MINIMUM_DISPLAY_APERTURE, coded)?,
            optional_aperture(media, &MF_MT_GEOMETRIC_APERTURE, coded)?,
        ];
        // 仅属性确实缺失时按 SDK 顺序选择；已存在但无效的 blob 必须报错。
        layout.visible = if layout.pan_scan_enabled {
            layout.apertures[0]
                .or(layout.apertures[1])
                .or(layout.apertures[2])
        } else {
            layout.apertures[1].or(layout.apertures[2])
        }
        .unwrap_or_else(|| Rect::full(coded.0, coded.1));
        Ok(layout)
    }

    fn new(coded: (u32, u32), aperture: Option<Rect>, default_stride: i32) -> Result<Self> {
        pixel_bytes(coded.0, coded.1)?;
        if default_stride == 0 || u64::from(default_stride.unsigned_abs()) < u64::from(coded.0) * 4
        {
            return Err("摄像头默认行跨度不足或无效".into());
        }
        let span = u64::from(default_stride.unsigned_abs()) * u64::from(coded.1 - 1)
            + u64::from(coded.0) * 4;
        if span > MAX_BUFFER_BYTES as u64 {
            return Err("摄像头默认行跨度超过缓冲预算".into());
        }
        let visible = aperture.unwrap_or_else(|| Rect::full(coded.0, coded.1));
        if !visible.valid(coded.0, coded.1) {
            return Err("摄像头显示区域超出当前帧".into());
        }
        Ok(Self {
            coded,
            visible,
            default_stride,
            frame_rate: None,
            pixel_aspect_ratio: (1, 1),
            interlace_mode: None,
            pan_scan_enabled: false,
            apertures: [None; 3],
        })
    }

    /// 变更标志伴随的样本不读取；owner 丢弃它并 fresh query 后决定是否恢复。
    pub(crate) fn read_sample(&self, sample: &IMFSample, flags: u32) -> Result<CameraFrame> {
        require_stable_sample(flags)?;
        if unsafe { sample.GetBufferCount()? } != 1 {
            return Err("摄像头 RGB32 样本须包含单个完整缓冲".into());
        }
        let buffer = unsafe { sample.GetBufferByIndex(0)? };
        let locked = LockedBuffer::read(buffer, self.default_stride, self.coded.1)?;
        self.validate_rows(locked.length, locked.first_row, locked.pitch)?;
        // 原生锁返回整个可访问范围；先检查数值边界，再创建只读 slice。
        if locked.base.is_null() || (locked.base as usize).checked_add(locked.length).is_none() {
            return Err("摄像头缓冲地址无效".into());
        }
        let bytes = unsafe { std::slice::from_raw_parts(locked.base, locked.length) };
        let frame = self.copy_rows(bytes, locked.first_row, locked.pitch)?;
        locked.finish()?;
        Ok(frame)
    }

    /// 对完整 coded frame 的首/末行先验范围；不会用缺损行填黑后宣称成功。
    fn validate_rows(&self, length: usize, first_row: usize, pitch: i32) -> Result<()> {
        let row = u64::from(self.coded.0) * 4;
        if length == 0
            || length > MAX_BUFFER_BYTES
            || pitch == 0
            || u64::from(pitch.unsigned_abs()) < row
        {
            return Err("摄像头缓冲长度或行跨度无效".into());
        }
        let first = first_row as i128;
        let last = first + i128::from(self.coded.1 - 1) * i128::from(pitch);
        let low = first.min(last);
        let high = first.max(last) + i128::from(row);
        if low < 0 || high > length as i128 || high - low > MAX_BUFFER_BYTES as i128 {
            return Err("摄像头帧的完整行范围超出缓冲".into());
        }
        Ok(())
    }

    fn copy_rows(&self, source: &[u8], first_row: usize, pitch: i32) -> Result<CameraFrame> {
        self.validate_rows(source.len(), first_row, pitch)?;
        let bytes = pixel_bytes(self.visible.width, self.visible.height)?;
        let mut bgra = Vec::new();
        bgra.try_reserve_exact(bytes)?;
        bgra.resize(bytes, 0);
        let row = self.visible.width as usize * 4;
        for y in 0..self.visible.height as usize {
            let start = first_row as i128
                + (i128::from(self.visible.y) + y as i128) * i128::from(pitch)
                + i128::from(self.visible.x) * 4;
            // coded 行与 aperture 都已完整验过，此处转换和切片不再容忍部分帧。
            let start = start as usize;
            bgra[y * row..(y + 1) * row].copy_from_slice(&source[start..start + row]);
        }
        for pixel in bgra.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        Ok(CameraFrame {
            width: self.visible.width,
            height: self.visible.height,
            bgra,
        })
    }
}

fn pixel_bytes(width: u32, height: u32) -> Result<usize> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0
        || height == 0
        || width > MAX_CODED_SIDE
        || height > MAX_CODED_SIDE
        || pixels > MAX_PIXELS
    {
        return Err("摄像头帧尺寸超出内部像素预算".into());
    }
    let bytes = usize::try_from(pixels.checked_mul(4).ok_or("摄像头像素字节数溢出")?)?;
    if bytes > MAX_DECODED_BYTES {
        return Err("摄像头帧超过 16 MiB 像素预算".into());
    }
    Ok(bytes)
}

fn require_stable_sample(flags: u32) -> Result<()> {
    let rejected = MF_SOURCE_READERF_ERROR.0
        | MF_SOURCE_READERF_NEWSTREAM.0
        | MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED.0
        | MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED.0
        | MF_SOURCE_READERF_ALLEFFECTSREMOVED.0;
    if flags & rejected as u32 != 0 {
        return Err(format!("摄像头样本携带错误或格式变化标志，未读取像素：0x{flags:08x}").into());
    }
    Ok(())
}

fn optional_u32(media: &IMFMediaType, key: &GUID) -> Result<Option<u32>> {
    match unsafe { media.GetUINT32(key) } {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn optional_u64(media: &IMFMediaType, key: &GUID) -> Result<Option<u64>> {
    match unsafe { media.GetUINT64(key) } {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn valid_ratio(value: u64) -> Result<(u32, u32)> {
    let ratio = ((value >> 32) as u32, value as u32);
    if ratio.0 == 0 || ratio.1 == 0 {
        return Err("摄像头类型中的比例必须为正".into());
    }
    Ok(ratio)
}

fn square_pixel_ratio(value: Option<u64>) -> Result<(u32, u32)> {
    let ratio = value.map(valid_ratio).transpose()?.unwrap_or((1, 1));
    if ratio.0 != ratio.1 {
        return Err("摄像头帧暂不接纳非正方形像素".into());
    }
    Ok(ratio)
}

fn require_zero_rotation(rotation: Option<u32>) -> Result<()> {
    if rotation.is_some_and(|value| value != 0) {
        return Err("摄像头帧暂不接纳非零或未知旋转".into());
    }
    Ok(())
}

fn area_rect(area: MFVideoArea, coded: (u32, u32)) -> Result<Rect> {
    if area.OffsetX.fract != 0
        || area.OffsetY.fract != 0
        || area.OffsetX.value < 0
        || area.OffsetY.value < 0
        || area.Area.cx <= 0
        || area.Area.cy <= 0
    {
        return Err("摄像头 aperture 必须为非负整数原点和正尺寸".into());
    }
    let rect = Rect {
        x: area.OffsetX.value as u32,
        y: area.OffsetY.value as u32,
        width: area.Area.cx as u32,
        height: area.Area.cy as u32,
    };
    if !rect.valid(coded.0, coded.1) {
        return Err("摄像头 aperture 超出当前帧".into());
    }
    Ok(rect)
}

fn optional_aperture(media: &IMFMediaType, key: &GUID, coded: (u32, u32)) -> Result<Option<Rect>> {
    let length = match unsafe { media.GetBlobSize(key) } {
        Ok(length) => length as usize,
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if length != size_of::<MFVideoArea>() {
        return Err("摄像头 aperture blob 长度无效".into());
    }
    let mut area = MFVideoArea::default();
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            (&mut area as *mut MFVideoArea).cast::<u8>(),
            size_of::<MFVideoArea>(),
        )
    };
    let mut actual = 0_u32;
    unsafe { media.GetBlob(key, bytes, Some(&mut actual))? };
    if actual as usize != length {
        return Err("摄像头 aperture blob 在读取时变化".into());
    }
    Ok(Some(area_rect(area, coded)?))
}

enum Unlock {
    Surface(IMF2DBuffer2),
    Linear(IMFMediaBuffer),
}

/// 每次成功原生 Lock 立即取得释放责任；显式 finish 报 Unlock 错误，早退由 Drop 释放。
struct LockedBuffer {
    base: *const u8,
    length: usize,
    first_row: usize,
    pitch: i32,
    unlock: Option<Unlock>,
}

impl LockedBuffer {
    fn read(buffer: IMFMediaBuffer, default_stride: i32, height: u32) -> Result<Self> {
        match buffer.cast::<IMF2DBuffer2>() {
            Ok(surface) => {
                let mut scan0 = ptr::null_mut();
                let mut base = ptr::null_mut();
                let mut pitch = 0_i32;
                let mut length = 0_u32;
                unsafe {
                    surface.Lock2DSize(
                        MF2DBuffer_LockFlags_Read,
                        &mut scan0,
                        &mut pitch,
                        &mut base,
                        &mut length,
                    )?;
                }
                let mut locked = Self {
                    base,
                    length: length as usize,
                    first_row: 0,
                    pitch,
                    unlock: Some(Unlock::Surface(surface)),
                };
                if scan0.is_null() || base.is_null() {
                    return Err("摄像头二维缓冲地址无效".into());
                }
                locked.first_row = (scan0 as usize)
                    .checked_sub(base as usize)
                    .ok_or("摄像头二维缓冲首行早于可访问范围")?;
                Ok(locked)
            }
            Err(error) if error.code() == E_NOINTERFACE => {
                // IMFMediaBuffer::Lock 提供连续表示；使用默认 signed stride，不推算 padding。
                // 只有旧 IMF2DBuffer 时也走此有明确长度的路径，不构造未知边界的裸 slice。
                let mut base = ptr::null_mut();
                let mut maximum = 0_u32;
                let mut length = 0_u32;
                unsafe { buffer.Lock(&mut base, Some(&mut maximum), Some(&mut length))? };
                let mut locked = Self {
                    base,
                    length: length as usize,
                    first_row: 0,
                    pitch: default_stride,
                    unlock: Some(Unlock::Linear(buffer)),
                };
                if length > maximum || maximum as usize > MAX_BUFFER_BYTES {
                    return Err("摄像头线性缓冲长度或容量超过预算".into());
                }
                if default_stride < 0 {
                    locked.first_row = (default_stride.unsigned_abs() as usize)
                        .checked_mul(height as usize - 1)
                        .ok_or("摄像头负行跨度首行计算溢出")?;
                }
                Ok(locked)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn finish(mut self) -> Result<()> {
        self.release()
    }

    fn release(&mut self) -> Result<()> {
        match self.unlock.take() {
            Some(Unlock::Surface(surface)) => unsafe { surface.Unlock2D()? },
            Some(Unlock::Linear(buffer)) => unsafe { buffer.Unlock()? },
            None => {}
        }
        Ok(())
    }
}

impl Drop for LockedBuffer {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// destination 与 region 都为全局物理像素；返回实际写入的 ROI 局部矩形。
/// 仅 fit 内的摄像头像素覆盖屏幕，留边与交集外像素保持原值；不做镜像或装饰。
pub(crate) fn composite_camera(
    target: &mut Image,
    region: ScreenRect,
    camera: &CameraFrame,
    destination: ScreenRect,
) -> Result<Option<Rect>> {
    let layout = RecordingSizeLayout::new(region)?;
    layout.validate_frame(target.width, target.height, target.bgra.len())?;
    if camera.bgra.len() != pixel_bytes(camera.width, camera.height)? {
        return Err("摄像头 BGRA 帧长度不匹配".into());
    }
    if destination.width == 0 || destination.height == 0 {
        return Err("摄像头画中画目标尺寸为空".into());
    }
    let (dw, dh) = (u64::from(destination.width), u64::from(destination.height));
    let (sw, sh) = (u64::from(camera.width), u64::from(camera.height));
    // 输入边已封顶，以下最大乘积小于 2^44；先求完整 fit 再 clip，不能裁后重新缩放。
    let (fit_width, fit_height) = if dw * sh <= dh * sw {
        (dw, dw * sh / sw)
    } else {
        (dh * sw / sh, dh)
    };
    if fit_width == 0 || fit_height == 0 {
        return Err("摄像头画中画按比例缩放后的尺寸为空".into());
    }
    let fit_left = i64::from(destination.left) + ((dw - fit_width) / 2) as i64;
    let fit_top = i64::from(destination.top) + ((dh - fit_height) / 2) as i64;
    let left = fit_left.max(i64::from(region.left));
    let top = fit_top.max(i64::from(region.top));
    let right = (fit_left + fit_width as i64).min(i64::from(region.left) + i64::from(region.width));
    let bottom =
        (fit_top + fit_height as i64).min(i64::from(region.top) + i64::from(region.height));
    if right <= left || bottom <= top {
        return Ok(None);
    }
    let clipped = Rect {
        x: (left - i64::from(region.left)) as u32,
        y: (top - i64::from(region.top)) as u32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    };
    if !clipped.valid(target.width, target.height) {
        return Err("摄像头画中画交集超出录制帧".into());
    }
    // 所有尺寸、完整缓冲和交集都已验证；此后才修改目标，且不修改任何全局几何。
    for y in 0..clipped.height {
        let sy = ((top - fit_top) as u64 + u64::from(y)) * sh / fit_height;
        for x in 0..clipped.width {
            let sx = ((left - fit_left) as u64 + u64::from(x)) * sw / fit_width;
            let source = (sy as usize * camera.width as usize + sx as usize) * 4;
            let output = (clipped.y as usize + y as usize) * layout.source_stride
                + (clipped.x as usize + x as usize) * 4;
            target.bgra[output..output + 3].copy_from_slice(&camera.bgra[source..source + 3]);
            target.bgra[output + 3] = 255;
        }
    }
    Ok(Some(clipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(width: u32, height: u32) -> CameraFrame {
        let mut bgra = Vec::new();
        for y in 0..height {
            for x in 0..width {
                bgra.extend_from_slice(&[(y * 10 + x) as u8, 20, 30, 0]);
            }
        }
        CameraFrame {
            width,
            height,
            bgra,
        }
    }

    fn image(width: u32, height: u32) -> Image {
        Image {
            width,
            height,
            bgra: vec![77; width as usize * height as usize * 4],
            readback_ms: 0.,
        }
    }

    fn blues(bytes: &[u8]) -> Vec<u8> {
        bytes.chunks_exact(4).map(|pixel| pixel[0]).collect()
    }

    #[test]
    fn recording_camera_frame_handles_padding_aperture_and_negative_stride() {
        let layout = CameraLayout::new(
            (3, 2),
            Some(Rect {
                x: 1,
                y: 0,
                width: 2,
                height: 2,
            }),
            12,
        )
        .unwrap();
        let frame = pixels(3, 2);
        let mut top_down = vec![99; 32];
        let mut bottom_up = vec![99; 32];
        for y in 0..2 {
            top_down[y * 16..y * 16 + 12].copy_from_slice(&frame.bgra[y * 12..(y + 1) * 12]);
            bottom_up[(1 - y) * 16..(1 - y) * 16 + 12]
                .copy_from_slice(&frame.bgra[y * 12..(y + 1) * 12]);
        }
        let first = layout.copy_rows(&top_down, 0, 16).unwrap();
        let second = layout.copy_rows(&bottom_up, 16, -16).unwrap();
        assert_eq!(first, second);
        assert_eq!((first.width, first.height), (2, 2));
        assert_eq!(blues(&first.bgra), [1, 2, 11, 12]);
        assert!(first.bgra.chunks_exact(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn recording_camera_frame_rejects_incomplete_rows_and_excessive_geometry() {
        let layout = CameraLayout::new((3, 2), None, 12).unwrap();
        assert!(layout.copy_rows(&[0; 28], 0, 16).is_ok());
        for (length, first, pitch) in [(27, 0, 16), (28, 0, -16), (28, 0, 8), (28, usize::MAX, 16)]
        {
            assert!(layout.copy_rows(&vec![0; length], first, pitch).is_err());
        }
        assert!(layout.validate_rows(MAX_BUFFER_BYTES + 1, 0, 16).is_err());
        assert!(CameraLayout::new((4096, 1024), None, 16_384).is_ok());
        for coded in [(0, 1), (4097, 1), (4096, 1025)] {
            assert!(CameraLayout::new(coded, None, 16_388).is_err());
        }
    }

    #[test]
    fn recording_camera_frame_rejects_unknown_orientation_aperture_and_change_flags() {
        let mut area = MFVideoArea::default();
        area.Area.cx = 2;
        area.Area.cy = 2;
        assert_eq!(area_rect(area, (4, 4)).unwrap(), Rect::full(2, 2));
        area.OffsetX.fract = 1;
        assert!(area_rect(area, (4, 4)).is_err());
        area.OffsetX.fract = 0;
        area.OffsetX.value = -1;
        assert!(area_rect(area, (4, 4)).is_err());
        area.OffsetX.value = 3;
        assert!(area_rect(area, (4, 4)).is_err());
        assert_eq!(square_pixel_ratio(None).unwrap(), (1, 1));
        for ratio in [0, (1_u64 << 32) | 2] {
            assert!(square_pixel_ratio(Some(ratio)).is_err());
        }
        require_zero_rotation(None).unwrap();
        for rotation in [90, 180, 270, u32::MAX] {
            assert!(require_zero_rotation(Some(rotation)).is_err());
        }
        require_stable_sample(0).unwrap();
        for flag in [
            MF_SOURCE_READERF_CURRENTMEDIATYPECHANGED,
            MF_SOURCE_READERF_NATIVEMEDIATYPECHANGED,
            MF_SOURCE_READERF_ALLEFFECTSREMOVED,
            MF_SOURCE_READERF_ERROR,
        ] {
            assert!(require_stable_sample(flag.0 as u32).is_err());
        }
    }

    #[test]
    fn recording_camera_frame_composite_clips_without_reanchoring_global_sampling() {
        let region = ScreenRect {
            left: -100,
            top: -20,
            width: 6,
            height: 4,
        };
        let destination = ScreenRect {
            left: -102,
            top: -20,
            width: 8,
            height: 4,
        };
        let camera = pixels(4, 2);
        let mut target = image(6, 4);
        assert_eq!(
            composite_camera(&mut target, region, &camera, destination).unwrap(),
            Some(Rect::full(6, 4))
        );
        assert_eq!(blues(&target.bgra[..24]), [1, 1, 2, 2, 3, 3]);
        assert_eq!(blues(&target.bgra[72..]), [11, 11, 12, 12, 13, 13]);
        let mut moved = image(6, 4);
        composite_camera(
            &mut moved,
            ScreenRect {
                left: 400,
                top: 80,
                ..region
            },
            &camera,
            ScreenRect {
                left: 398,
                top: 80,
                ..destination
            },
        )
        .unwrap();
        assert_eq!(moved.bgra, target.bgra);
        assert_eq!((region.left, region.top), (-100, -20));
    }

    #[test]
    fn recording_camera_frame_composite_fits_and_validates_before_mutating() {
        let region = ScreenRect {
            left: -5,
            top: 3,
            width: 6,
            height: 4,
        };
        let mut target = image(6, 4);
        let camera = pixels(2, 2);
        let clipped = composite_camera(&mut target, region, &camera, region).unwrap();
        assert_eq!(
            clipped,
            Some(Rect {
                x: 1,
                y: 0,
                width: 4,
                height: 4
            })
        );
        for row in target.bgra.chunks_exact(24) {
            assert_eq!(&row[..4], &[77; 4]);
            assert_eq!(&row[20..], &[77; 4]);
        }
        let before = target.bgra.clone();
        let mut broken = camera.clone();
        broken.bgra.pop();
        assert!(composite_camera(&mut target, region, &broken, region).is_err());
        assert!(composite_camera(
            &mut target,
            region,
            &camera,
            ScreenRect { width: 0, ..region }
        )
        .is_err());
        assert_eq!(
            composite_camera(
                &mut target,
                region,
                &camera,
                ScreenRect {
                    left: 100,
                    ..region
                }
            )
            .unwrap(),
            None
        );
        assert_eq!(target.bgra, before);
        target.bgra.pop();
        let truncated = target.bgra.clone();
        assert!(composite_camera(&mut target, region, &camera, region).is_err());
        assert_eq!(target.bgra, truncated);
    }
}
