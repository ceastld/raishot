//! 实时贴图只保存窗口身份和相对客户区范围；不接收或转发源窗口输入。
use crate::{geometry::ScreenRect, pin_model::Placement};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceIdentity {
    pub hwnd: usize,
    pub process_id: u32,
    pub thread_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceState {
    Live,
    Minimized,
    Cloaked,
    Lost,
    Unavailable,
}

impl SourceState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Live => tr!("实时窗口", "Live window"),
            Self::Minimized => tr!("源窗口已最小化 · 冻结画面", "Minimized · frozen"),
            Self::Cloaked => tr!("源窗口不可见 · 冻结画面", "Hidden · frozen"),
            Self::Lost => tr!("源窗口已关闭 · 冻结画面", "Closed · frozen"),
            Self::Unavailable => tr!("实时画面不可用 · 冻结画面", "Unavailable · frozen"),
        }
    }
}

pub(crate) fn intersection(a: ScreenRect, b: ScreenRect) -> Option<ScreenRect> {
    let left = i64::from(a.left).max(i64::from(b.left));
    let top = i64::from(a.top).max(i64::from(b.top));
    let right =
        (i64::from(a.left) + i64::from(a.width)).min(i64::from(b.left) + i64::from(b.width));
    let bottom =
        (i64::from(a.top) + i64::from(a.height)).min(i64::from(b.top) + i64::from(b.height));
    (right > left && bottom > top).then(|| ScreenRect {
        left: left as i32,
        top: top as i32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

/// 裁剪后范围相对客户区固定，源窗口移动时不用重算选择本身。
pub(crate) fn client_region(selection: ScreenRect, client: ScreenRect) -> Option<ScreenRect> {
    let clipped = intersection(selection, client)?;
    Some(ScreenRect {
        left: clipped.left.checked_sub(client.left)?,
        top: clipped.top.checked_sub(client.top)?,
        ..clipped
    })
}

pub(crate) fn source_region(
    region: ScreenRect,
    client: ScreenRect,
    window: ScreenRect,
    extended: ScreenRect,
    source_size: (u32, u32),
) -> Option<ScreenRect> {
    let base = if source_size == (window.width, window.height) {
        window
    } else if source_size == (extended.width, extended.height) {
        extended
    } else {
        // 未知 DPI/边框口径拒绝投影，不能默默缩放导致区域漂移。
        return None;
    };
    let global = ScreenRect {
        left: client.left.checked_add(region.left)?,
        top: client.top.checked_add(region.top)?,
        width: region.width,
        height: region.height,
    };
    let clipped = intersection(global, client)?;
    if clipped != global {
        return None;
    }
    Some(ScreenRect {
        left: global.left.checked_sub(base.left)?,
        top: global.top.checked_sub(base.top)?,
        ..global
    })
}

pub(crate) fn bounded_zoom(size: (u32, u32), requested: f32) -> f32 {
    if size.0 == 0 || size.1 == 0 || !requested.is_finite() {
        return 1.;
    }
    let maximum = 5_f64
        .min(8192. / f64::from(size.0.max(size.1)))
        .min((8_000_000. / (f64::from(size.0) * f64::from(size.1))).sqrt());
    f64::from(requested).clamp(0.1_f64.min(maximum), maximum) as f32
}

/// 指针下的图像点固定，跨屏负坐标也不随缩放漂移。
pub(crate) fn zoom_placement(
    placement: Placement,
    size: (u32, u32),
    requested: f32,
    pointer: (i32, i32),
) -> (Placement, f32) {
    let zoom = bounded_zoom(size, requested);
    let width = (size.0 as f64 * zoom as f64).round().max(1.) as u32;
    let height = (size.1 as f64 * zoom as f64).round().max(1.) as u32;
    let x =
        (i64::from(pointer.0) - i64::from(placement.x)) as f64 / f64::from(placement.width.max(1));
    let y =
        (i64::from(pointer.1) - i64::from(placement.y)) as f64 / f64::from(placement.height.max(1));
    (
        Placement {
            x: (f64::from(pointer.0) - x * f64::from(width))
                .round()
                .clamp(i32::MIN as f64, i32::MAX as f64) as i32,
            y: (f64::from(pointer.1) - y * f64::from(height))
                .round()
                .clamp(i32::MIN as f64, i32::MAX as f64) as i32,
            width,
            height,
        },
        zoom,
    )
}

#[cfg(test)]
#[path = "pin_live_model_tests.rs"]
mod tests;
