//! EXR按首个平面RGB层解码；有界自有浮点图完成后复制到调用方缓冲。
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};

use exr::prelude::*;
use image::{ColorType, ImageDecoder, ImageError, ImageFormat, ImageResult};

/// 单次解块线程的显式栈容量，必须由调用方计入解码阶段工作集。
pub(super) const DECODER_THREAD_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Window {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// 有界前检批准的几何与通道选择。压缩、目录及各阶段容量仍由许可闭包核查。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Selection {
    pub part_index: usize,
    pub display_window: Window,
    pub data_window: Window,
    pub has_alpha: bool,
}

impl Selection {
    fn channels(self) -> usize {
        if self.has_alpha {
            4
        } else {
            3
        }
    }

    fn native_bytes(self) -> crate::Result<u64> {
        let dimensions = (self.display_window.width, self.display_window.height);
        let bytes = u64::from(dimensions.0)
            .checked_mul(u64::from(dimensions.1))
            .and_then(|pixels| pixels.checked_mul(self.channels() as u64 * 4))
            .ok_or_else(|| invalid("EXR像素容量溢出", "EXR pixel capacity overflow"))?;
        super::check_dimensions(dimensions, bytes)?;
        // 后端的data坐标也必须可按平台整数索引，不能先窄化再做窗口减法。
        if self.data_window.width == 0
            || self.data_window.height == 0
            || usize::try_from(self.data_window.width).is_err()
            || usize::try_from(self.data_window.height).is_err()
        {
            return Err(invalid("EXR数据窗口无效", "Invalid EXR data window").into());
        }
        Ok(bytes)
    }
}

fn invalid(zh: &'static str, en: &'static str) -> ImageError {
    ImageError::Decoding(image::error::DecodingError::new(
        ImageFormat::OpenExr.into(),
        crate::i18n::tr(zh, en),
    ))
}

fn cancelled(cancel: &AtomicBool) -> ImageResult<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(ImageError::IoError(io::Error::other(tr!(
            "图片读取已取消",
            "Image reading canceled"
        ))))
    } else {
        Ok(())
    }
}

fn backend_error(error: exr::error::Error) -> ImageError {
    ImageError::Decoding(image::error::DecodingError::new(
        ImageFormat::OpenExr.into(),
        error.to_string(),
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PixelFailure {
    Allocation,
    Capacity,
    Index,
}

/// 像素builder要求可跨任意reader借用期的自有storage；这里显式保留第二份native。
/// 创建闭包不能返回Result，故保存固定错误，setter不写入失败storage，最终整体返回Err。
struct OwnedPixelStorage {
    values: Vec<f32>,
    failure: Option<PixelFailure>,
}

impl OwnedPixelStorage {
    fn failed(failure: PixelFailure) -> Self {
        Self {
            values: Vec::new(),
            failure: Some(failure),
        }
    }

    fn new(samples: usize) -> Self {
        let mut values = Vec::new();
        if values.try_reserve_exact(samples).is_err() {
            return Self::failed(PixelFailure::Allocation);
        }
        // 预算只批准精确N；分配器给出更大的可见capacity时不继续使用未计费的图。
        if values.capacity() > samples {
            return Self::failed(PixelFailure::Capacity);
        }
        values.resize(samples, 0.0);
        Self {
            values,
            failure: None,
        }
    }

    fn set(&mut self, selection: Selection, position: Vec2<usize>, pixel: [f32; 4]) {
        if self.failure.is_some() {
            return;
        }
        let Some(index) = display_pixel(selection, position.x(), position.y()) else {
            return;
        };
        let Some(start) = index.checked_mul(selection.channels()) else {
            self.failure = Some(PixelFailure::Index);
            return;
        };
        let Some(end) = start.checked_add(selection.channels()) else {
            self.failure = Some(PixelFailure::Index);
            return;
        };
        let Some(target) = self.values.get_mut(start..end) else {
            self.failure = Some(PixelFailure::Index);
            return;
        };
        target.copy_from_slice(&pixel[..selection.channels()]);
    }

    fn copy_to(self, bytes: &mut [u8], cancel: &AtomicBool) -> ImageResult<()> {
        cancelled(cancel)?;
        if let Some(failure) = self.failure {
            return Err(match failure {
                PixelFailure::Allocation => {
                    invalid("无法分配EXR像素缓冲", "Cannot allocate EXR pixel buffer")
                }
                PixelFailure::Capacity => invalid(
                    "EXR像素实际容量超过预算",
                    "EXR pixel capacity exceeds its budget",
                ),
                PixelFailure::Index => invalid("EXR像素索引无效", "Invalid EXR pixel index"),
            });
        }
        if self.values.len().checked_mul(4) != Some(bytes.len()) {
            return Err(invalid(
                "EXR输出缓冲大小不符",
                "EXR output buffer size mismatch",
            ));
        }
        for (samples, target) in self
            .values
            .chunks(16 * 1024)
            .zip(bytes.chunks_mut(64 * 1024))
        {
            cancelled(cancel)?;
            for (sample, target) in samples.iter().zip(target.chunks_exact_mut(4)) {
                target.copy_from_slice(&sample.to_ne_bytes());
            }
        }
        cancelled(cancel)
    }
}

/// 只借用同一冻结来源；每次读取和定位都检查取消，不把Interrupted作为取消信号。
#[derive(Debug)]
struct CancelCursor<'a> {
    cursor: Cursor<&'a [u8]>,
    cancel: &'a AtomicBool,
}

impl CancelCursor<'_> {
    fn check(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(io::Error::other(tr!(
                "图片读取已取消",
                "Image reading canceled"
            )))
        } else {
            Ok(())
        }
    }
}

impl Read for CancelCursor<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let result = self.cursor.read(bytes);
        self.check()?;
        result
    }
}

impl Seek for CancelCursor<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let result = self.cursor.seek(position);
        self.check()?;
        result
    }
}

/// 不自行放宽前检。许可闭包须在任何后端目录分配之前证明来源、元数据、
/// filter/块解压、调用方native、第二份私有native和DECODER_THREAD_BYTES均容纳于预算。
pub(super) struct Decoder<'a> {
    chunks: exr::block::reader::Reader<CancelCursor<'a>>,
    selection: Selection,
    cancel: &'a AtomicBool,
}

impl<'a> Decoder<'a> {
    pub(super) fn new(
        bytes: &'a [u8],
        cancel: &'a AtomicBool,
        approve: impl FnOnce(&[u8], &AtomicBool) -> crate::Result<Selection>,
    ) -> crate::Result<Self> {
        let result = (|| {
            cancelled(cancel)?;
            let selection = approve(bytes, cancel)?;
            cancelled(cancel)?;
            selection.native_bytes()?;
            let chunks = exr::block::read(
                CancelCursor {
                    cursor: Cursor::new(bytes),
                    cancel,
                },
                false,
            )
            .map_err(backend_error)?;
            if selected(chunks.headers())? != selection {
                return Err(invalid(
                    "EXR后端选择与已批准来源不一致",
                    "EXR backend selection differs from the approved source",
                )
                .into());
            }
            Ok(Self {
                chunks,
                selection,
                cancel,
            })
        })();
        cancelled(cancel)?;
        result
    }

    fn read_serial(self, bytes: &mut [u8]) -> ImageResult<()> {
        cancelled(self.cancel)?;
        let selection = self.selection;
        let samples = bytes.len() / 4;
        let cancel = self.cancel;
        // 两个move闭包仅捕获Copy标量，满足builder高阶ReadLayers约束。
        // storage全0初始化保留display空白（RGBA透明、RGB黑）；不借用caller或取消令牌。
        let decoded = read()
            .no_deep_data()
            .largest_resolution_level()
            .rgba_channels(
                move |_size, _channels| OwnedPixelStorage::new(samples),
                move |storage: &mut OwnedPixelStorage,
                      position,
                      (r, g, b, a): (f32, f32, f32, f32)| {
                    storage.set(selection, position, [r, g, b, a]);
                },
            )
            .first_valid_layer()
            .all_attributes()
            .non_parallel()
            .from_chunks(self.chunks)
            .map_err(backend_error);
        cancelled(cancel)?;
        // 元数据及块缓冲在此块结束时销毁；只保留自有native，按64KiB向caller复制。
        let pixels = {
            let decoded = decoded?;
            decoded.layer_data.channel_data.pixels
        };
        pixels.copy_to(bytes, cancel)
    }
}

/// i64容纳窗口坐标差；负offset或完全不相交均按display裁切，不重定位源像素。
fn display_pixel(selection: Selection, x: usize, y: usize) -> Option<usize> {
    let x = i64::try_from(x)
        .ok()?
        .checked_add(i64::from(selection.data_window.x))?
        .checked_sub(i64::from(selection.display_window.x))?;
    let y = i64::try_from(y)
        .ok()?
        .checked_add(i64::from(selection.data_window.y))?
        .checked_sub(i64::from(selection.display_window.y))?;
    if x < 0
        || y < 0
        || x >= i64::from(selection.display_window.width)
        || y >= i64::from(selection.display_window.height)
    {
        return None;
    }
    (y as usize)
        .checked_mul(selection.display_window.width as usize)?
        .checked_add(x as usize)
}

fn selected(headers: &[exr::meta::header::Header]) -> ImageResult<Selection> {
    let (part_index, header) = headers
        .iter()
        .enumerate()
        .find(|(_, header)| {
            !header.deep
                && ["R", "G", "B"].iter().all(|&name| {
                    header
                        .channels
                        .find_index_of_channel(&Text::from(name))
                        .is_some()
                })
        })
        .ok_or_else(|| invalid("EXR不含平面RGB层", "EXR has no flat RGB layer"))?;
    let display = header.shared_attributes.display_window;
    let convert = |position: Vec2<i32>, size: Vec2<usize>| -> ImageResult<Window> {
        Ok(Window {
            x: position.x(),
            y: position.y(),
            width: u32::try_from(size.width())
                .map_err(|_| invalid("EXR窗口过大", "EXR window is too large"))?,
            height: u32::try_from(size.height())
                .map_err(|_| invalid("EXR窗口过大", "EXR window is too large"))?,
        })
    };
    Ok(Selection {
        part_index,
        display_window: convert(display.position, display.size)?,
        data_window: convert(header.own_attributes.layer_position, header.layer_size)?,
        has_alpha: header
            .channels
            .find_index_of_channel(&Text::from("A"))
            .is_some(),
    })
}

impl ImageDecoder for Decoder<'_> {
    fn dimensions(&self) -> (u32, u32) {
        (
            self.selection.display_window.width,
            self.selection.display_window.height,
        )
    }

    fn color_type(&self) -> ColorType {
        if self.selection.has_alpha {
            ColorType::Rgba32F
        } else {
            ColorType::Rgb32F
        }
    }

    fn read_image(self, bytes: &mut [u8]) -> ImageResult<()> {
        let cancel = self.cancel;
        let result = (|| {
            cancelled(cancel)?;
            if self.total_bytes() != bytes.len() as u64 {
                return Err(invalid(
                    "EXR输出缓冲大小不符",
                    "EXR output buffer size mismatch",
                ));
            }
            // 单次线程隔离解块TLS缓存；显式join使线程、栈和缓存先于返回释放。
            std::thread::scope(|scope| {
                let thread = std::thread::Builder::new()
                    .stack_size(DECODER_THREAD_BYTES as usize)
                    .spawn_scoped(scope, move || self.read_serial(bytes))
                    .map_err(ImageError::IoError)?;
                thread
                    .join()
                    .map_err(|_| invalid("EXR解码线程失败", "EXR decode thread failed"))?
            })
        })();
        cancelled(cancel)?;
        result
    }

    fn read_image_boxed(self: Box<Self>, bytes: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(bytes)
    }
}

#[cfg(test)]
#[path = "image_decode_exr_adapter_tests.rs"]
mod tests;
