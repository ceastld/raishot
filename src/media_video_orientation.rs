//! 在解码边界统一视频像素方向，窗口与帧输出继续消费同一份 BGRA。
use crate::{geometry::Rotation, Result};
use windows::{
    core::{Interface, GUID},
    Win32::Media::MediaFoundation::{
        IMFMediaType, IMFSourceReader, IMFSourceReaderEx, IMFTransform, IMFVideoProcessorControl,
        MFMediaType_Video, MFVideoFormat_RGB32, MFT_CATEGORY_VIDEO_PROCESSOR,
        MF_E_ATTRIBUTENOTFOUND, MF_E_INVALIDINDEX, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
        MF_MT_VIDEO_ROTATION, MF_SOURCE_READER_FIRST_VIDEO_STREAM, MF_XVP_DISABLE_FRC,
        ROTATION_NONE,
    },
};

const MAX_TRANSFORMS: u32 = 32;
const MAX_FRAME_PIXELS: u64 = 64_000_000;

/// 保留容器角度，但只在处理器已明确关闭方向纠正后才旋转非零角度的像素。
#[derive(Clone, Debug)]
pub(crate) struct VideoOrientation {
    raw_degrees: u32,
    correction: Rotation,
    frc_disabled: bool,
    /// 只保留首次流送前已配置的对象；流中不再次枚举或重配置处理链。
    processors: Vec<IMFTransform>,
}

impl VideoOrientation {
    /// 在输出类型协商完成后、首次 ReadSample 前调用；接口随 Reader 在同一 MF 线程释放。
    /// 调用方重新配置处理链后必须重新建立这个约定，不能沿用旧对象。
    pub(crate) fn prepare(
        reader: &IMFSourceReader,
        raw_degrees: u32,
        advanced: bool,
    ) -> Result<Self> {
        let mut orientation = Self::from_degrees(raw_degrees)?;
        if raw_degrees == 0 && !advanced {
            return Ok(orientation);
        }
        let mut configured = Vec::new();
        let processors = visit_processors(reader, |transform| {
            if advanced {
                // 播放按源 PTS 调度；XVP 的默认帧率转换会把变帧率改成名义帧率。
                let attributes = unsafe { transform.GetAttributes()? };
                unsafe { attributes.SetUINT32(&MF_XVP_DISABLE_FRC, 1)? };
                if unsafe { attributes.GetUINT32(&MF_XVP_DISABLE_FRC)? } != 1 {
                    return Err(tr!(
                        "无法关闭视频帧率转换",
                        "Cannot disable video frame-rate conversion"
                    )
                    .into());
                }
            }
            if raw_degrees != 0 {
                let control = transform
                    .cast::<IMFVideoProcessorControl>()
                    .map_err(|error| {
                        trf!(
                            "视频处理器不支持明确关闭旋转：{error}",
                            "The video processor cannot disable rotation explicitly: {error}"
                        )
                    })?;
                unsafe { control.SetRotation(ROTATION_NONE) }.map_err(|error| {
                    trf!(
                        "无法关闭视频处理器的旋转纠正：{error}",
                        "Cannot disable video processor rotation correction: {error}"
                    )
                })?;
            }
            configured.push(transform.clone());
            Ok(())
        })?;
        if raw_degrees != 0 && processors == 0 {
            return Err(tr!(
                "未找到可明确关闭旋转的视频处理器",
                "No video processor was found for explicit rotation control"
            )
            .into());
        }
        orientation.frc_disabled = advanced && processors > 0;
        orientation.processors = configured;
        Ok(orientation)
    }

    pub(crate) fn frame_rate_conversion_disabled(&self) -> bool {
        self.frc_disabled
    }

    /// CURRENTMEDIATYPECHANGED 是输出协商，不等于处理链移除。
    /// 调用方先拒绝原生类型变化和 ALLEFFECTSREMOVED，再只读核对既有处理约定。
    pub(crate) fn validate_current_type(
        &self,
        reader: &IMFSourceReader,
        media: &IMFMediaType,
    ) -> Result<()> {
        let major = unsafe { media.GetGUID(&MF_MT_MAJOR_TYPE)? };
        let subtype = unsafe { media.GetGUID(&MF_MT_SUBTYPE)? };
        if major != MFMediaType_Video || subtype != MFVideoFormat_RGB32 {
            return Err(trf!(
                "视频输出不再是 RGB32：major={major:?}, subtype={subtype:?}",
                "Video output is no longer RGB32: major={major:?}, subtype={subtype:?}"
            )
            .into());
        }
        let native =
            unsafe { reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)? };
        self.validate_rotation_metadata(media_rotation(&native)?, media_rotation(media)?)?;
        if self.frc_disabled {
            for processor in &self.processors {
                let attributes = unsafe { processor.GetAttributes()? };
                if unsafe { attributes.GetUINT32(&MF_XVP_DISABLE_FRC)? } != 1 {
                    return Err(tr!(
                        "视频处理器不再保持禁用帧率转换",
                        "The video processor no longer has frame-rate conversion disabled"
                    )
                    .into());
                }
            }
        }
        Ok(())
    }

    fn validate_rotation_metadata(&self, source: u32, output: u32) -> Result<()> {
        // 输出可以保留源方向标签，也可以不再附带标签；像素纠正仍只由明确
        // ROTATION_NONE 的既有处理器约定和本对象执行，不从宽高或标签猜测已旋转。
        if source != self.raw_degrees || (output != 0 && output != self.raw_degrees) {
            return Err(trf!(
                "视频方向元数据变化：expected={}, source={source}, output={output}",
                "Video orientation metadata changed: expected={}, source={source}, output={output}",
                self.raw_degrees
            )
            .into());
        }
        Ok(())
    }

    fn from_degrees(raw_degrees: u32) -> Result<Self> {
        // MF 属性描述内容已逆时针旋转的角度；Rotation 的 R90/R270 输出为顺时针，
        // 因而同名枚举在这里恰好执行反方向纠正，不是再次应用原始内容方向。
        let correction = match raw_degrees {
            0 => Rotation::Identity,
            90 => Rotation::R90,
            180 => Rotation::R180,
            270 => Rotation::R270,
            _ => {
                return Err(trf!(
                    "不支持的视频旋转角度：{raw_degrees}°",
                    "Unsupported video rotation: {raw_degrees} degrees"
                )
                .into());
            }
        };
        Ok(Self {
            raw_degrees,
            correction,
            frc_disabled: false,
            processors: Vec::new(),
        })
    }

    pub(crate) fn raw_degrees(&self) -> u32 {
        self.raw_degrees
    }

    /// 输入为去除孔径外填充后的原始显示区域；输出为归一后的公开尺寸。
    pub(crate) fn size(&self, raw_width: u32, raw_height: u32) -> Result<(u32, u32)> {
        frame_bytes(raw_width, raw_height)?;
        Ok(self.correction.size(raw_width, raw_height))
    }

    /// 每个原始帧只调用一次。先由解码层处理孔径和 pitch，再交这里纠正方向。
    pub(crate) fn apply(&self, bgra: Vec<u8>, raw_width: u32, raw_height: u32) -> Result<Vec<u8>> {
        let expected = frame_bytes(raw_width, raw_height)?;
        if bgra.len() != expected {
            return Err(tr!(
                "视频帧像素长度与原始尺寸不符",
                "The video frame length does not match its original size"
            )
            .into());
        }
        if self.raw_degrees == 0 {
            return Ok(bgra);
        }
        let (width, height) = self.correction.size(raw_width, raw_height);
        let mut output = Vec::new();
        output.try_reserve_exact(expected)?;
        output.resize(expected, 0);
        for y in 0..height {
            for x in 0..width {
                let (source_x, source_y) = self.correction.raw_pixel(x, y, raw_width, raw_height);
                let source = (source_y as usize * raw_width as usize + source_x as usize) * 4;
                let destination = (y as usize * width as usize + x as usize) * 4;
                output[destination..destination + 4].copy_from_slice(&bgra[source..source + 4]);
            }
        }
        Ok(output)
    }
}

fn media_rotation(media: &IMFMediaType) -> Result<u32> {
    match unsafe { media.GetUINT32(&MF_MT_VIDEO_ROTATION) } {
        Ok(rotation) => Ok(rotation),
        Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn visit_processors(
    reader: &IMFSourceReader,
    mut visit: impl FnMut(&IMFTransform) -> Result<()>,
) -> Result<u32> {
    let reader = reader.cast::<IMFSourceReaderEx>().map_err(|error| {
        trf!(
            "无法检查视频处理链：{error}",
            "Cannot inspect video processing: {error}"
        )
    })?;
    let mut count = 0;
    for index in 0..MAX_TRANSFORMS {
        let mut category = GUID::zeroed();
        let mut transform = None;
        match unsafe {
            reader.GetTransformForStream(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                index,
                Some(&mut category),
                &mut transform,
            )
        } {
            Ok(()) => {}
            Err(error) if error.code() == MF_E_INVALIDINDEX => return Ok(count),
            Err(error) => {
                return Err(trf!(
                    "无法检查视频处理链：{error}",
                    "Cannot inspect video processing: {error}"
                )
                .into())
            }
        }
        if category == MFT_CATEGORY_VIDEO_PROCESSOR {
            let transform = transform.ok_or(tr!(
                "视频处理器未返回有效接口",
                "The video processor returned no interface"
            ))?;
            visit(&transform)?;
            count += 1;
        }
    }
    Err(tr!("视频处理链过长", "The video processing chain is too long").into())
}

fn frame_bytes(width: u32, height: u32) -> Result<usize> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels == 0 || pixels > MAX_FRAME_PIXELS {
        return Err(tr!(
            "视频帧尺寸无效或过大",
            "The video frame size is invalid or too large"
        )
        .into());
    }
    Ok(usize::try_from(pixels * 4)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(labels: &[u8]) -> Vec<u8> {
        labels
            .iter()
            .flat_map(|label| [*label, label.wrapping_add(20), label.wrapping_add(40), 128])
            .collect()
    }

    #[test]
    fn corrects_all_quarter_turns_with_asymmetric_non_square_pixels() {
        // 原始两列三行：AB / CD / EF。MF 90 表示内容已逆时针，纠正后为 ECA / FDB。
        for (degrees, dimensions, expected) in [
            (0, (2, 3), b"ABCDEF".as_slice()),
            (90, (3, 2), b"ECAFDB".as_slice()),
            (180, (2, 3), b"FEDCBA".as_slice()),
            (270, (3, 2), b"BDFACE".as_slice()),
        ] {
            let orientation = VideoOrientation::from_degrees(degrees).unwrap();
            assert_eq!(orientation.raw_degrees(), degrees);
            assert_eq!(orientation.size(2, 3).unwrap(), dimensions);
            assert_eq!(
                orientation.apply(pixels(b"ABCDEF"), 2, 3).unwrap(),
                pixels(expected)
            );
        }
    }

    #[test]
    fn zero_rotation_keeps_the_owned_buffer() {
        let orientation = VideoOrientation::from_degrees(0).unwrap();
        let original = pixels(b"ABCDEF");
        let address = original.as_ptr();
        let output = orientation.apply(original, 2, 3).unwrap();
        assert_eq!(output.as_ptr(), address);
        assert_eq!(output, pixels(b"ABCDEF"));
    }

    #[test]
    fn rejects_angles_outside_the_metadata_enum() {
        for degrees in [1, 45, 89, 91, 360, u32::MAX] {
            assert!(VideoOrientation::from_degrees(degrees).is_err());
        }
    }

    #[test]
    fn output_negotiation_keeps_the_original_rotation_contract() {
        for degrees in [0, 90, 180, 270] {
            let orientation = VideoOrientation::from_degrees(degrees).unwrap();
            // 标签传播与像素旋转是不同的事；保持源方向时两种输出标签都兼容。
            assert!(orientation.validate_rotation_metadata(degrees, 0).is_ok());
            assert!(orientation
                .validate_rotation_metadata(degrees, degrees)
                .is_ok());
            let changed = (degrees + 90) % 360;
            assert!(orientation
                .validate_rotation_metadata(changed, degrees)
                .is_err());
            for output in [90, 180, 270] {
                if output != degrees {
                    assert!(orientation
                        .validate_rotation_metadata(degrees, output)
                        .is_err());
                }
            }
        }
    }

    #[test]
    fn rejects_bad_dimensions_without_allocating_frame_storage() {
        let orientation = VideoOrientation::from_degrees(90).unwrap();
        for (width, height) in [(0, 3), (2, 0), (u32::MAX, u32::MAX), (8_001, 8_000)] {
            assert!(orientation.size(width, height).is_err());
            assert!(orientation.apply(Vec::new(), width, height).is_err());
        }
        assert_eq!(orientation.size(8_000, 8_000).unwrap(), (8_000, 8_000));
    }

    #[test]
    fn rejects_incomplete_and_excess_pixels_before_indexing() {
        for degrees in [0, 90, 180, 270] {
            let orientation = VideoOrientation::from_degrees(degrees).unwrap();
            for length in [0, 1, 23, 25, 28] {
                assert!(orientation.apply(vec![0; length], 2, 3).is_err());
            }
        }
    }
}
