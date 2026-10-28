//! 仅本次截图会话使用的不可变原生 DIB/DDB；不转移捕获 HDC/DC，也不缓存待机屏幕。
use crate::{
    geometry::{Rect, ScreenRect},
    Result,
};
use std::{
    mem::size_of,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
};
use windows::Win32::Graphics::Gdi::*;

const MAX_BITMAP_BYTES: usize = 256 * 1024 * 1024;
const MAX_BITMAP_SIDE: u32 = 16_384;

pub(crate) struct StartupBitmap {
    bitmap_id: usize,
    bounds: ScreenRect,
    source_bytes: usize,
    regions: Vec<Rect>,
    whole_canvas_black_gaps_valid: bool,
    selection: Mutex<()>,
    selected_in_owned_dc: AtomicBool,
    blits: AtomicUsize,
    batches: AtomicUsize,
    last_batch_planes: AtomicUsize,
    trace: bool,
}

fn layout_bytes(bounds: ScreenRect, regions: &[Rect]) -> Result<usize> {
    let bytes = (bounds.width as usize)
        .checked_mul(bounds.height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or("首屏 DIB 像素长度溢出")?;
    if bounds.width == 0
        || bounds.height == 0
        || bounds.width > MAX_BITMAP_SIDE
        || bounds.height > MAX_BITMAP_SIDE
        || bytes > MAX_BITMAP_BYTES
        || bounds.left.checked_add_unsigned(bounds.width).is_none()
        || bounds.top.checked_add_unsigned(bounds.height).is_none()
        || regions.is_empty()
        || regions
            .iter()
            .any(|rect| !rect.valid(bounds.width, bounds.height))
    {
        return Err("首屏 DIB 尺寸、坐标、区域或像素预算无效".into());
    }
    Ok(bytes)
}

impl StartupBitmap {
    /// 先取得句柄所有权再校验，校验失败时由 Drop 回收。
    fn owning(bitmap: HBITMAP, bounds: ScreenRect, regions: Vec<Rect>) -> Self {
        Self {
            bitmap_id: bitmap.0 as usize,
            bounds,
            source_bytes: 0,
            regions,
            whole_canvas_black_gaps_valid: false,
            selection: Mutex::new(()),
            selected_in_owned_dc: AtomicBool::new(false),
            blits: AtomicUsize::new(0),
            batches: AtomicUsize::new(0),
            last_batch_planes: AtomicUsize::new(0),
            trace: std::env::var_os("RUSTSHOT_STARTUP_TRACE").is_some(),
        }
    }
    /// 消费唯一 bitmap 所有权。调用者必须先 GdiFlush，并确认已从所有 DC 脱离。
    /// 调用者必须证明原 CreateDIBSection 输入是 32 位 BI_RGB 且
    /// biHeight == -bounds.height 的 top-down DIB；GetObject 返回的正高度不能证明方向。
    /// 即使校验失败，本方法也负责回收 bitmap；不能把它再次交还原捕获 surface。
    pub(crate) unsafe fn from_deselected(
        bitmap: HBITMAP,
        bounds: ScreenRect,
        regions: Vec<Rect>,
    ) -> Result<Self> {
        let mut owner = Self::owning(bitmap, bounds, regions);
        if bitmap.is_invalid() {
            return Err("首屏 DIB 原生句柄无效".into());
        }
        owner.source_bytes = layout_bytes(bounds, &owner.regions)?;
        let mut header = DIBSECTION::default();
        let read = unsafe {
            GetObjectW(
                bitmap.into(),
                size_of::<DIBSECTION>() as i32,
                Some((&mut header as *mut DIBSECTION).cast()),
            )
        };
        if read != size_of::<DIBSECTION>() as i32
            || header.dsBm.bmWidth != bounds.width as i32
            || header.dsBm.bmHeight != bounds.height as i32
            || header.dsBm.bmWidthBytes != (bounds.width * 4) as i32
            || header.dsBm.bmPlanes != 1
            || header.dsBm.bmBitsPixel != 32
            || header.dsBm.bmBits.is_null()
            || header.dsBmih.biSize != size_of::<BITMAPINFOHEADER>() as u32
            || header.dsBmih.biWidth != bounds.width as i32
            // 自有 top-down DIB 的 GetObject 会将高度规范化为正数；方向依赖上述创建来源。
            || header.dsBmih.biHeight != bounds.height as i32
            || header.dsBmih.biPlanes != 1
            || header.dsBmih.biBitCount != 32
            || header.dsBmih.biCompression != BI_RGB.0
        {
            return Err("首屏 DIB 原生头、top-down BGRA 或行跨度不符".into());
        }
        Ok(owner)
    }

    /// 自有测试图案入口；仅复制调用者提供的 BGRA，不读取桌面。
    pub(crate) fn from_pixels(bounds: ScreenRect, regions: Vec<Rect>, bgra: &[u8]) -> Result<Self> {
        let bytes = layout_bytes(bounds, &regions)?;
        if bgra.len() != bytes {
            return Err("首屏自有 DIB 像素长度不符".into());
        }
        let info = crate::gdi_surface::bitmap_info(bounds.width, bounds.height);
        let mut bits = std::ptr::null_mut();
        let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)? };
        // 新 DIB 没有选入任何 DC；校验失败也由 owner 清理。
        let owner = unsafe { Self::from_deselected(bitmap, bounds, regions)? };
        if bits.is_null() {
            return Err("首屏自有 DIB 没有返回像素地址".into());
        }
        unsafe {
            GdiFlush().ok()?;
            std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), bytes);
        }
        Ok(owner)
    }

    pub(crate) fn bounds(&self) -> ScreenRect {
        self.bounds
    }
    pub(crate) fn source_bytes(&self) -> usize {
        self.source_bytes
    }
    /// 捕获时冻结的显示器局部区域；顺序与 CPU planes 一致。
    pub(crate) fn regions(&self) -> &[Rect] {
        &self.regions
    }
    /// 仅读原 bitmap 头，不读取像素；失败必须由原生启动校验拒绝。
    pub(crate) fn kind(&self) -> &'static str {
        let mut header = BITMAP::default();
        let read = unsafe {
            GetObjectW(
                HGDIOBJ(self.bitmap_id as *mut _),
                size_of::<BITMAP>() as i32,
                Some((&mut header as *mut BITMAP).cast()),
            )
        };
        if read != size_of::<BITMAP>() as i32 {
            "invalid"
        } else if header.bmBits.is_null() {
            "ddb"
        } else {
            "dib"
        }
    }
    /// 消费已从原线程 DC 反选、flush 的独立显示缓存 DDB。
    /// 原捕获 DDB 另由 PendingStartupCapture 拥有，读回不会锁住此缓存。
    /// source_bytes 是 BGRA 等价容量，不是驱动/GPU 实际分配量。
    pub(crate) unsafe fn from_deselected_ddb(
        bitmap: HBITMAP,
        bounds: ScreenRect,
        regions: Vec<Rect>,
    ) -> Result<Self> {
        let mut owner = Self::owning(bitmap, bounds, regions);
        if bitmap.is_invalid() {
            return Err("首屏 DDB 原生句柄无效".into());
        }
        owner.source_bytes = layout_bytes(bounds, &owner.regions)?;
        let mut header = BITMAP::default();
        let read = unsafe {
            GetObjectW(
                bitmap.into(),
                size_of::<BITMAP>() as i32,
                Some((&mut header as *mut BITMAP).cast()),
            )
        };
        if read != size_of::<BITMAP>() as i32
            || header.bmWidth != bounds.width as i32
            || header.bmHeight != bounds.height as i32
            || header.bmPlanes != 1
            || header.bmBitsPixel != 32
            || !header.bmBits.is_null()
        {
            return Err("首屏 DDB 原生尺寸/颜色格式/非 DIB 身份不符".into());
        }
        Ok(owner)
    }
    /// 仅接受所有画布像素已初始化、regions 内为同次冻结图的独立缓存。
    /// 调用者仍须满足 from_deselected_ddb 的 flush、反选及唯一所有权要求；
    /// 未覆盖的屏间空隙必须保持 BGR 全黑，不能仅凭 DDB 类型作此保证。
    pub(crate) unsafe fn from_deselected_ddb_with_black_gaps(
        bitmap: HBITMAP,
        bounds: ScreenRect,
        regions: Vec<Rect>,
    ) -> Result<Self> {
        let mut owner = unsafe { Self::from_deselected_ddb(bitmap, bounds, regions)? };
        owner.whole_canvas_black_gaps_valid = true;
        Ok(owner)
    }
    pub(crate) fn whole_canvas_black_gaps_valid(&self) -> bool {
        self.whole_canvas_black_gaps_valid
    }
    /// 仅完整且已清黑空隙的缓存可覆盖同尺寸物理视口；不改变原 regions。
    pub(crate) fn blit_full_canvas_to(&self, destination: HDC, viewport: ScreenRect) -> Result<()> {
        if destination.is_invalid()
            || !self.whole_canvas_black_gaps_valid
            || viewport != self.bounds
        {
            return Err("首屏整画布缺少已验证黑色空隙或物理视口不符".into());
        }
        let full = Rect::full(self.bounds.width, self.bounds.height);
        self.blit_validated(destination, &[(full, full)])
    }
    pub(crate) fn object_id(&self) -> usize {
        self.bitmap_id
    }
    pub(crate) fn blit_count(&self) -> usize {
        self.blits.load(Ordering::Acquire)
    }
    pub(crate) fn batch_count(&self) -> usize {
        self.batches.load(Ordering::Acquire)
    }
    pub(crate) fn last_batch_planes(&self) -> usize {
        self.last_batch_planes.load(Ordering::Acquire)
    }

    /// 只在调用线程临时选入 DC；仅原监视器内的局部像素可读，屏间空隙由调用者填黑。
    pub(crate) fn blit_to(&self, destination: HDC, planes: &[(Rect, Rect)]) -> Result<()> {
        if destination.is_invalid()
            || planes.is_empty()
            || planes.iter().any(|&(source, dest)| {
                !source.valid(self.bounds.width, self.bounds.height)
                    || source.width != dest.width
                    || source.height != dest.height
                    || dest
                        .x
                        .checked_add(dest.width)
                        .is_none_or(|v| v > i32::MAX as u32)
                    || dest
                        .y
                        .checked_add(dest.height)
                        .is_none_or(|v| v > i32::MAX as u32)
                    || !self.regions.iter().any(|region| {
                        source.x >= region.x
                            && source.y >= region.y
                            && source.x + source.width <= region.x + region.width
                            && source.y + source.height <= region.y + region.height
                    })
            })
        {
            return Err("首屏 DIB BitBlt 源、目标或已捕获区域不符".into());
        }
        self.blit_validated(destination, planes)
    }

    fn blit_validated(&self, destination: HDC, planes: &[(Rect, Rect)]) -> Result<()> {
        let _selection = self.selection.lock().map_err(|_| "首屏 DIB 选择锁已失效")?;
        if self.selected_in_owned_dc.load(Ordering::Acquire) {
            return Err("首屏 DIB 上一次临时 DC 未能回收，拒绝重复选择".into());
        }
        let selected = SelectedDc::new(
            destination,
            HBITMAP(self.bitmap_id as *mut _),
            &self.selected_in_owned_dc,
        )?;
        let copied = (|| -> Result<()> {
            unsafe {
                // 整批先验证；同一源 DC 连续绘制各屏，最后统一提交。
                for &(source, dest) in planes {
                    BitBlt(
                        destination,
                        dest.x as i32,
                        dest.y as i32,
                        dest.width as i32,
                        dest.height as i32,
                        Some(selected.dc),
                        source.x as i32,
                        source.y as i32,
                        SRCCOPY,
                    )?;
                }
                GdiFlush().ok()?;
            }
            Ok(())
        })();
        // 即使 BitBlt/Flush 失败，恢复/删除仍先于释放选择锁。
        let released = selected.finish();
        match (copied, released) {
            (Err(copy), Err(cleanup)) => {
                Err(format!("首屏 DIB 绘制失败：{copy}；回收失败：{cleanup}").into())
            }
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => {
                self.blits.fetch_add(planes.len(), Ordering::AcqRel);
                self.last_batch_planes
                    .store(planes.len(), Ordering::Release);
                self.batches.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }
        }
    }
}

impl Drop for StartupBitmap {
    fn drop(&mut self) {
        if HBITMAP(self.bitmap_id as *mut _).is_invalid() {
            return;
        }
        let not_selected = !self.selected_in_owned_dc.load(Ordering::Acquire);
        let deleted = unsafe { DeleteObject(HGDIOBJ(self.bitmap_id as *mut _)) }.as_bool();
        if self.trace || !deleted {
            println!("{{\"phase\":\"startup_bitmap_drop\",\"pid\":{},\"bitmap_id\":\"{}\",\"source_bytes\":{},\"blit_count\":{},\"bitmap_not_selected_in_owned_dc\":{not_selected},\"delete_ok\":{deleted}}}",
                std::process::id(), self.object_id(), self.source_bytes(), self.blit_count());
        }
    }
}

struct SelectedDc<'a> {
    dc: HDC,
    previous: HGDIOBJ,
    selected: HGDIOBJ,
    selection_state: &'a AtomicBool,
}
impl<'a> SelectedDc<'a> {
    fn new(destination: HDC, bitmap: HBITMAP, selection_state: &'a AtomicBool) -> Result<Self> {
        let dc = unsafe { CreateCompatibleDC(Some(destination)) };
        if dc.is_invalid() {
            return Err(windows::core::Error::from_win32().into());
        }
        let mut owner = Self {
            dc,
            previous: HGDIOBJ::default(),
            selected: bitmap.into(),
            selection_state,
        };
        owner.previous = unsafe { SelectObject(dc, owner.selected) };
        if owner.previous.is_invalid() {
            return Err("首屏 DIB 临时选择失败".into());
        }
        selection_state.store(true, Ordering::Release);
        Ok(owner)
    }

    fn release(&mut self) -> (bool, bool) {
        if self.dc.is_invalid() {
            return (true, true);
        }
        let restored = if self.previous.is_invalid() {
            true
        } else {
            let displaced = unsafe { SelectObject(self.dc, self.previous) };
            let restored = displaced == self.selected
                && unsafe { GetCurrentObject(self.dc, OBJ_BITMAP) } == self.previous;
            if restored {
                self.previous = HGDIOBJ::default();
                self.selection_state.store(false, Ordering::Release);
            }
            restored
        };
        let deleted = unsafe { DeleteDC(self.dc) }.as_bool();
        if deleted {
            self.dc = HDC::default();
            self.previous = HGDIOBJ::default();
            self.selection_state.store(false, Ordering::Release);
        }
        (restored, deleted)
    }

    fn finish(mut self) -> Result<()> {
        let (restored, deleted) = self.release();
        if !restored || !deleted {
            return Err("首屏 DIB 临时 DC 恢复或删除失败".into());
        }
        Ok(())
    }
}
impl Drop for SelectedDc<'_> {
    fn drop(&mut self) {
        let _ = self.release();
    }
}
