//! 截图贴图的不可变 CPU 采样域；像素平面、光标与可编辑文档分别保存。
use crate::{
    capture::Image,
    cursor::CapturedCursor,
    geometry::{Rect, ScreenRect},
    scene::{PixelPlane, SourcePixels},
    Result,
};
use gpui::RenderImage;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[path = "pin_capture_source_store.rs"]
pub(crate) mod resource;

#[cfg(test)]
#[path = "pin_capture_test_fixture.rs"]
pub(crate) mod test_fixture;

pub(crate) const MAX_CONTEXT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PLANES: usize = 64;
const MAX_CURSOR_PIXELS: u64 = 16_384;

fn rect_model(value: [u32; 4]) -> Rect {
    Rect {
        x: value[0],
        y: value[1],
        width: value[2],
        height: value[3],
    }
}
fn rect_wire(rect: Rect) -> [u32; 4] {
    [rect.x, rect.y, rect.width, rect.height]
}
fn validate_bounds(bounds: ScreenRect) -> Result<()> {
    if bounds.width == 0
        || bounds.height == 0
        || bounds.width > 65_535
        || bounds.height > 65_535
        || i64::from(bounds.left) + i64::from(bounds.width) > i64::from(i32::MAX)
        || i64::from(bounds.top) + i64::from(bounds.height) > i64::from(i32::MAX)
    {
        return Err("截图采样域尺寸或有符号原点无效".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CursorPixels {
    rect: [u32; 4],
    bgra: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CaptureSourceMetadata {
    version: u32,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    content: [u32; 4],
    fill: Option<[u8; 4]>,
    planes: Vec<[u32; 4]>,
    cursor: Option<CursorPixels>,
}
impl CaptureSourceMetadata {
    pub(crate) fn new(
        pixels: &CapturePixels,
        content: Rect,
        cursor: Option<&CapturedCursor>,
    ) -> Result<Self> {
        pixels.validate()?;
        let bounds = pixels.bounds;
        if let Some(cursor) = cursor {
            if !cursor.rect.valid(bounds.width, bounds.height)
                || (cursor.image.width, cursor.image.height)
                    != (cursor.rect.width, cursor.rect.height)
                || u64::from(cursor.rect.width) * u64::from(cursor.rect.height) > MAX_CURSOR_PIXELS
                || cursor.image.bgra.len() as u64
                    != u64::from(cursor.rect.width) * u64::from(cursor.rect.height) * 4
            {
                return Err("截图独立光标无效或超出64 KiB预算".into());
            }
        }
        let metadata = Self {
            version: 1,
            left: bounds.left,
            top: bounds.top,
            width: bounds.width,
            height: bounds.height,
            content: rect_wire(content),
            fill: pixels.fill,
            planes: pixels.planes.iter().map(|p| rect_wire(p.rect)).collect(),
            cursor: cursor.map(|c| CursorPixels {
                rect: rect_wire(c.rect),
                bgra: c.image.bgra.clone(),
            }),
        };
        metadata.validate()?;
        Ok(metadata)
    }
    pub(crate) fn bounds(&self) -> ScreenRect {
        ScreenRect {
            left: self.left,
            top: self.top,
            width: self.width,
            height: self.height,
        }
    }
    pub(crate) fn content_rect(&self) -> Rect {
        rect_model(self.content)
    }
    pub(crate) fn storage_size(&self) -> (u32, u32) {
        (self.content[2], self.content[3])
    }
    pub(crate) fn validate(&self) -> Result<()> {
        validate_bounds(self.bounds())?;
        let content = self.content_rect();
        if self.version != 1
            || !content.valid(self.width, self.height)
            || u64::from(content.width) * u64::from(content.height) > 32_000_000
            || self.planes.len() > MAX_PLANES
            || self.planes.is_empty() && self.fill.is_none()
        {
            return Err("截图贴图来源版本、可见范围或平面数量无效".into());
        }
        let bytes = self
            .planes
            .iter()
            .try_fold(0u64, |sum, &p| -> Result<u64> {
                let p = rect_model(p);
                if !p.valid(self.width, self.height) {
                    return Err("截图采样平面超出完整来源域".into());
                }
                sum.checked_add(u64::from(p.width) * u64::from(p.height) * 4)
                    .ok_or_else(|| "截图采样平面容量溢出".into())
            })?;
        if bytes > MAX_CONTEXT_BYTES {
            return Err("截图 CPU 采样上下文超过 256 MiB".into());
        }
        if let Some(cursor) = &self.cursor {
            let rect = rect_model(cursor.rect);
            if !rect.valid(self.width, self.height)
                || u64::from(rect.width) * u64::from(rect.height) > MAX_CURSOR_PIXELS
                || cursor.bgra.len() as u64 != u64::from(rect.width) * u64::from(rect.height) * 4
            {
                return Err("截图独立光标尺寸、范围或像素长度无效".into());
            }
        }
        Ok(())
    }
    pub(crate) fn cursor_model(&self) -> Option<CapturedCursor> {
        self.cursor.as_ref().map(|c| {
            let rect = rect_model(c.rect);
            CapturedCursor {
                rect,
                image: Image {
                    width: rect.width,
                    height: rect.height,
                    bgra: c.bgra.clone(),
                    readback_ms: 0.,
                },
            }
        })
    }
    pub(crate) fn matches(&self, pixels: &CapturePixels) -> bool {
        self.bounds() == pixels.bounds
            && self.fill == pixels.fill
            && self
                .planes
                .iter()
                .copied()
                .map(rect_model)
                .eq(pixels.planes.iter().map(|p| p.rect))
    }
    pub(crate) fn plane_rects(&self) -> impl Iterator<Item = Rect> + '_ {
        self.planes.iter().copied().map(rect_model)
    }
    pub(crate) fn fill(&self) -> Option<[u8; 4]> {
        self.fill
    }
}

#[derive(Clone)]
pub(crate) struct OwnedPlane {
    pub(crate) rect: Rect,
    pub(crate) image: Arc<RenderImage>,
}
#[derive(Clone)]
pub(crate) struct CapturePixels {
    pub(crate) bounds: ScreenRect,
    pub(crate) fill: Option<[u8; 4]>,
    pub(crate) planes: Vec<OwnedPlane>,
}
impl CapturePixels {
    pub(crate) fn new(
        bounds: ScreenRect,
        fill: Option<[u8; 4]>,
        planes: Vec<OwnedPlane>,
    ) -> Result<Self> {
        let pixels = Self {
            bounds,
            fill,
            planes,
        };
        pixels.validate()?;
        Ok(pixels)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        validate_bounds(self.bounds)?;
        if self.planes.len() > MAX_PLANES || self.planes.is_empty() && self.fill.is_none() {
            return Err("截图 CPU 来源缺少有效像素平面".into());
        }
        let bytes = self.planes.iter().try_fold(0u64, |sum, p| -> Result<u64> {
            let rect = p.rect;
            if p.image.frame_count() != 1 {
                return Err("截图CPU平面帧格式无效".into());
            }
            let frame = p.image.size(0);
            let expected = u64::from(rect.width) * u64::from(rect.height) * 4;
            if p.image.frame_count() != 1
                || !rect.valid(self.bounds.width, self.bounds.height)
                || (u32::from(frame.width), u32::from(frame.height)) != (rect.width, rect.height)
                || p.image.as_bytes(0).map(<[u8]>::len) != usize::try_from(expected).ok()
            {
                return Err("截图 CPU 平面尺寸、stride 或像素长度无效".into());
            }
            sum.checked_add(expected)
                .ok_or_else(|| "截图 CPU 上下文容量溢出".into())
        })?;
        if bytes > MAX_CONTEXT_BYTES {
            return Err("截图 CPU 采样上下文超过 256 MiB".into());
        }
        Ok(())
    }
    pub(crate) fn source_pixels(&self) -> SourcePixels<'_> {
        SourcePixels {
            canvas: Rect::full(self.bounds.width, self.bounds.height),
            fill: self.fill,
            planes: self
                .planes
                .iter()
                .map(|p| PixelPlane {
                    rect: p.rect,
                    bgra: p.image.as_bytes(0).expect("validated immutable CPU plane"),
                })
                .collect(),
            overlay: None,
        }
    }
    pub(crate) fn read_content(&self, content: Rect) -> Result<Image> {
        if !content.valid(self.bounds.width, self.bounds.height)
            || u64::from(content.width) * u64::from(content.height) > 32_000_000
        {
            return Err("截图内容输出超出来源或 3200 万像素预算".into());
        }
        Ok(self.source_pixels().read(content))
    }
    /// 按行核对兼容PNG与不可变来源，不为校验再分配一张选区图。
    pub(crate) fn content_matches(&self, content: Rect, bgra: &[u8]) -> bool {
        if !content.valid(self.bounds.width, self.bounds.height)
            || bgra.len() as u64 != u64::from(content.width) * u64::from(content.height) * 4
        {
            return false;
        }
        let source = self.source_pixels();
        for (row, actual) in bgra.chunks_exact(content.width as usize * 4).enumerate() {
            let expected = source.read(Rect {
                x: content.x,
                y: content.y + row as u32,
                width: content.width,
                height: 1,
            });
            if expected.bgra != actual {
                return false;
            }
        }
        true
    }
}

pub(crate) fn deserialize_source<'de, D>(
    d: D,
) -> std::result::Result<Option<CaptureSourceMetadata>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    CaptureSourceMetadata::deserialize(d).map(Some)
}

/// renderer只在真实整帧Present成功后发布此物理尺寸，准备/上传失败或仅边框均为0。
pub(crate) fn content_presented(marker: &std::sync::atomic::AtomicU64, host: (u32, u32)) -> bool {
    host.0 > 0
        && host.1 > 0
        && marker.load(std::sync::atomic::Ordering::Acquire)
            == ((u64::from(host.0) << 32) | u64::from(host.1))
}
