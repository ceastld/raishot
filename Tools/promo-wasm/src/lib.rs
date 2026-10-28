//! 宣传页演示用 WebAssembly 外壳。
//! 拼接、对齐、运动预测与固定栏检测原样引用 `src/` 下的生产源码，这里只补它们依赖的
//! 最小类型（Image、Rect、Result、文案宏），并导出给页面调用的 C ABI。
#![allow(dead_code)]

macro_rules! tr {
    ($zh:literal, $en:literal $(,)?) => {
        $zh
    };
}
macro_rules! trf {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        format!($zh $(, $arg)*)
    };
}

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub mod capture {
    pub struct Image {
        pub width: u32,
        pub height: u32,
        pub bgra: Vec<u8>,
        pub readback_ms: f64,
    }
}

pub mod geometry {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Rect {
        pub x: u32,
        pub y: u32,
        pub width: u32,
        pub height: u32,
    }
    impl Rect {
        pub fn full(width: u32, height: u32) -> Self {
            Self {
                x: 0,
                y: 0,
                width,
                height,
            }
        }
        pub fn valid(self, w: u32, h: u32) -> bool {
            self.width > 0
                && self.height > 0
                && self.x.checked_add(self.width).is_some_and(|v| v <= w)
                && self.y.checked_add(self.height).is_some_and(|v| v <= h)
        }
    }
}

#[path = "../../../src/seam_alignment.rs"]
mod seam_alignment;
#[path = "../../../src/stitch.rs"]
pub mod stitch;
#[path = "../../../src/stitch_content.rs"]
mod stitch_content;

use capture::Image;
use std::{cell::RefCell, sync::Arc};
use stitch::{ScrollAxis, StitchOptions, StitchSnapshot, StitchUpdate, Stitcher, UpdateKind};
use stitch_content::StableContentBorderTracker;

/// 原生基准与页面共用的会话：一个 Stitcher 加上只用于展示的固定栏探测。
pub struct Session {
    pub stitcher: Stitcher,
    axis: ScrollAxis,
    staged: Option<Arc<Image>>,
    current: Arc<Image>,
    update: Option<StitchUpdate>,
    snapshot: Option<StitchSnapshot>,
    probe_prev: Arc<Image>,
    probe: StableContentBorderTracker,
}
impl Session {
    pub fn new(image: Image, axis: ScrollAxis, detect_borders: bool) -> Result<Self> {
        let image = Arc::new(image);
        let stitcher = Stitcher::with_options_shared(
            image.clone(),
            StitchOptions {
                axis,
                detect_fixed_borders: detect_borders,
                ..Default::default()
            },
        )?;
        Ok(Self {
            stitcher,
            axis,
            staged: None,
            current: image.clone(),
            update: None,
            snapshot: None,
            probe_prev: image,
            probe: StableContentBorderTracker::default(),
        })
    }
    /// 只计拼接本身；像素拷入由 stage 完成，对应产品里采集线程交出的帧。
    pub fn process(&mut self, image: Arc<Image>) -> i32 {
        self.snapshot = None;
        self.current = image.clone();
        match self.stitcher.process_shared(image) {
            Ok(update) => {
                let code = kind_code(update.kind);
                self.update = Some(update);
                code
            }
            Err(_) => {
                self.update = None;
                -1
            }
        }
    }
    /// 最近一次处理的位移与追加像素，供原生基准生成结果指纹。
    pub fn last_update(&self) -> (i32, u32) {
        self.update.as_ref().map_or((0, 0), |u| (u.delta, u.added))
    }
    fn snapshot(&mut self) -> &StitchSnapshot {
        self.snapshot
            .get_or_insert_with(|| self.stitcher.snapshot())
    }
}
fn kind_code(kind: UpdateKind) -> i32 {
    match kind {
        UpdateKind::Unchanged => 0,
        UpdateKind::ViewportOnly => 1,
        UpdateKind::Added => 2,
        UpdateKind::Rejected => 3,
        UpdateKind::Limit => 4,
    }
}
fn along(axis: ScrollAxis, rect: geometry::Rect) -> (u32, u32) {
    match axis {
        ScrollAxis::Vertical => (rect.y, rect.height),
        ScrollAxis::Horizontal => (rect.x, rect.width),
    }
}

thread_local! {
    static SESSIONS: RefCell<Vec<Option<Session>>> = const { RefCell::new(Vec::new()) };
}
fn with<T>(handle: i32, default: T, f: impl FnOnce(&mut Session) -> T) -> T {
    SESSIONS.with(|s| {
        s.borrow_mut()
            .get_mut(handle as usize)
            .and_then(Option::as_mut)
            .map_or(default, f)
    })
}
unsafe fn read_image(width: u32, height: u32, ptr: *const u8) -> Image {
    let len = width as usize * height as usize * 4;
    Image {
        width,
        height,
        bgra: std::slice::from_raw_parts(ptr, len).to_vec(),
        readback_ms: 0.,
    }
}

#[no_mangle]
pub extern "C" fn buf_alloc(len: usize) -> *mut u8 {
    let mut buffer = Vec::<u8>::with_capacity(len);
    let ptr = buffer.as_mut_ptr();
    std::mem::forget(buffer);
    ptr
}
/// # Safety
/// `ptr` 必须来自同一 `len` 的 `buf_alloc`。
#[no_mangle]
pub unsafe extern "C" fn buf_free(ptr: *mut u8, len: usize) {
    drop(Vec::from_raw_parts(ptr, 0, len));
}
/// # Safety
/// `ptr` 指向 `width*height*4` 字节的帧。
#[no_mangle]
pub unsafe extern "C" fn st_new(
    width: u32,
    height: u32,
    ptr: *const u8,
    horizontal: u32,
    detect_borders: u32,
) -> i32 {
    let axis = if horizontal != 0 {
        ScrollAxis::Horizontal
    } else {
        ScrollAxis::Vertical
    };
    let Ok(session) = Session::new(read_image(width, height, ptr), axis, detect_borders != 0)
    else {
        return -1;
    };
    SESSIONS.with(|s| {
        let mut sessions = s.borrow_mut();
        if let Some(index) = sessions.iter().position(Option::is_none) {
            sessions[index] = Some(session);
            index as i32
        } else {
            sessions.push(Some(session));
            sessions.len() as i32 - 1
        }
    })
}
#[no_mangle]
pub extern "C" fn st_free(handle: i32) {
    SESSIONS.with(|s| {
        if let Some(slot) = s.borrow_mut().get_mut(handle as usize) {
            *slot = None;
        }
    });
}
/// # Safety
/// `ptr` 指向与首帧同尺寸的帧。
#[no_mangle]
pub unsafe extern "C" fn st_stage(handle: i32, ptr: *const u8) {
    with(handle, (), |s| {
        let image = read_image(s.current.width, s.current.height, ptr);
        s.staged = Some(Arc::new(image));
    });
}
/// 处理 stage 的帧，返回 0 不变、1 仅视口、2 追加、3 拒绝、4 上限、-1 错误。
#[no_mangle]
pub extern "C" fn st_process(handle: i32) -> i32 {
    with(handle, -1, |s| match s.staged.take() {
        Some(image) => s.process(image),
        None => -1,
    })
}
#[no_mangle]
pub extern "C" fn st_delta(handle: i32) -> i32 {
    with(handle, 0, |s| s.update.as_ref().map_or(0, |u| u.delta))
}
#[no_mangle]
pub extern "C" fn st_added(handle: i32) -> u32 {
    with(handle, 0, |s| s.update.as_ref().map_or(0, |u| u.added))
}
#[no_mangle]
pub extern "C" fn st_view_start(handle: i32) -> u32 {
    with(handle, 0, |s| {
        let axis = s.axis;
        along(axis, s.snapshot().viewport).0
    })
}
#[no_mangle]
pub extern "C" fn st_length(handle: i32) -> u32 {
    with(handle, 0, |s| match s.axis {
        ScrollAxis::Vertical => s.stitcher.height,
        ScrollAxis::Horizontal => s.stitcher.width,
    })
}
#[no_mangle]
pub extern "C" fn st_seg_count(handle: i32) -> u32 {
    with(handle, 0, |s| s.snapshot().segments.len() as u32)
}
#[no_mangle]
pub extern "C" fn st_seg_origin(handle: i32, index: u32) -> u32 {
    with(handle, 0, |s| {
        let axis = s.axis;
        s.snapshot()
            .segments
            .get(index as usize)
            .map_or(0, |g| match axis {
                ScrollAxis::Vertical => g.origin.1,
                ScrollAxis::Horizontal => g.origin.0,
            })
    })
}
#[no_mangle]
pub extern "C" fn st_seg_len(handle: i32, index: u32) -> u32 {
    with(handle, 0, |s| {
        let axis = s.axis;
        s.snapshot()
            .segments
            .get(index as usize)
            .map_or(0, |g| along(axis, g.source).1)
    })
}
/// 条带源矩形左上角像素地址；行跨度为 `st_seg_stride` 字节。
#[no_mangle]
pub extern "C" fn st_seg_ptr(handle: i32, index: u32) -> *const u8 {
    with(handle, std::ptr::null(), |s| {
        s.snapshot()
            .segments
            .get(index as usize)
            .map_or(std::ptr::null(), |g| {
                let offset =
                    (g.source.y as usize * g.image.width as usize + g.source.x as usize) * 4;
                g.image.bgra[offset..].as_ptr()
            })
    })
}
#[no_mangle]
pub extern "C" fn st_seg_stride(handle: i32, index: u32) -> u32 {
    with(handle, 0, |s| {
        s.snapshot()
            .segments
            .get(index as usize)
            .map_or(0, |g| g.image.width * 4)
    })
}
#[no_mangle]
pub extern "C" fn st_undo(handle: i32) -> u32 {
    with(handle, 0, |s| {
        s.snapshot = None;
        u32::from(s.stitcher.undo())
    })
}
/// 展示用：用同一固定栏检测器观察上一帧与当前帧，返回 `top << 16 | bottom`；不计入拼接耗时。
#[no_mangle]
pub extern "C" fn st_observe_borders(handle: i32) -> u32 {
    with(handle, 0, |s| {
        if !Arc::ptr_eq(&s.probe_prev, &s.current) {
            s.probe
                .observe_frame_pair(&s.probe_prev, &s.current, s.axis);
            s.probe_prev = s.current.clone();
        }
        s.probe
            .fixed_borders()
            .map_or(0, |b| (b.top.min(0xffff) << 16) | b.bottom.min(0xffff))
    })
}
