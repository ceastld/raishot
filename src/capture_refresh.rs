//! 显式刷新只准备新底图；选区、文档和窗口的事务安装由截图宿主负责。
use crate::{
    canvas_source::CanvasSource,
    capture::cancellation::CaptureCancelled,
    capture_lock::NativeCaptureCancelToken,
    cursor::{CapturedCursor, PendingCursorSnapshot},
    fast_capture::FrozenDesktop,
    geometry::{Rect, ScreenRect},
    scene::{PixelPlane, SourcePixels},
    selection_assist::WindowTarget,
    Result,
};
use gpui::RenderImage;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use windows::Win32::Graphics::Dwm::DwmFlush;

/// 保留各屏顺序：工具栏锚点和预览平面仍使用原显示器索引。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RefreshTopology {
    bounds: ScreenRect,
    screens: Vec<Rect>,
}

impl RefreshTopology {
    pub(crate) fn of(source: &CanvasSource) -> Self {
        Self {
            bounds: source.bounds,
            screens: (0..source.screen_count())
                .map(|index| source.screen_rect(index))
                .collect(),
        }
    }

    pub(crate) fn matches(&self, source: &CanvasSource) -> bool {
        *self == Self::of(source)
    }

    fn validate(&self, source: &CanvasSource) -> Result<()> {
        if !self.matches(source) {
            return Err(tr!(
                "屏幕布局已改变，当前截图已保留；请重新开始截图",
                "Display layout changed; capture kept. Start a new capture"
            )
            .into());
        }
        Ok(())
    }
}

pub(crate) struct PreparedRefresh {
    pub(crate) source: CanvasSource,
    pub(crate) previews: Vec<Arc<RenderImage>>,
    pub(crate) cursor: Option<CapturedCursor>,
    pub(crate) window_targets: Vec<WindowTarget>,
}

fn check_cancelled(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(CaptureCancelled.into())
    } else {
        Ok(())
    }
}

fn refresh_escape_edge(held: &AtomicBool, down: bool, plain: bool) -> bool {
    let previous = held.swap(down, Ordering::AcqRel);
    down && !previous && plain
}

fn planes<'a>(
    source: &CanvasSource,
    previews: &'a [Arc<RenderImage>],
) -> Result<Vec<PixelPlane<'a>>> {
    if source.screen_count() != previews.len() {
        return Err(tr!(
            "刷新截图的屏幕预览不完整",
            "Refreshed screen previews are incomplete"
        )
        .into());
    }
    previews
        .iter()
        .enumerate()
        .map(|(index, preview)| {
            let rect = source.screen_rect(index);
            let size = preview.size(0);
            let bgra = preview.as_bytes(0).ok_or(tr!(
                "刷新截图没有有效像素",
                "Refreshed capture has no valid pixels"
            ))?;
            if !rect.valid(source.bounds.width, source.bounds.height)
                || u32::from(size.width) != rect.width
                || u32::from(size.height) != rect.height
                || bgra.len() as u64 != u64::from(rect.width) * u64::from(rect.height) * 4
            {
                return Err(tr!(
                    "刷新截图的像素尺寸不一致",
                    "Refreshed capture pixel sizes do not match"
                )
                .into());
            }
            Ok(PixelPlane { rect, bgra })
        })
        .collect()
}

/// 仅在后台调用。宿主须先同步隐藏截图层及辅助窗口，并保持隐藏直至此函数返回。
/// DwmFlush 等待隐藏真正进入桌面合成，不在 UI 线程阻塞；本函数从不创建覆盖窗。
/// 单次取消与整个截图会话分开，取消刷新不会撤销 CaptureLock。
pub(crate) fn prepare(
    expected: RefreshTopology,
    session: NativeCaptureCancelToken,
    request_cancelled: Arc<AtomicBool>,
    escape_was_down: bool,
) -> Result<PreparedRefresh> {
    let escape_held = AtomicBool::new(escape_was_down);
    let cancelled = || {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
        };
        let escape_down = unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) } < 0;
        let plain = [VK_SHIFT, VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
            .into_iter()
            .all(|key| unsafe { GetAsyncKeyState(key.0 as i32) } >= 0);
        if refresh_escape_edge(&escape_held, escape_down, plain) {
            request_cancelled.store(true, Ordering::Release);
        }
        request_cancelled.load(Ordering::Acquire) || !session.session_is_live()
    };
    check_cancelled(&cancelled)?;
    unsafe { DwmFlush()? };
    check_cancelled(&cancelled)?;
    let pending_cursor = PendingCursorSnapshot::begin();
    let (source, previews) = match crate::gpui_capture::prepare_dxgi_cancelled(&cancelled) {
        Ok((source, previews, _)) => (source, previews),
        Err(error) if !cancelled() && crate::capture::can_fallback_refresh(error.as_ref()) => {
            let frozen = FrozenDesktop::capture()?;
            check_cancelled(&cancelled)?;
            CanvasSource::frozen_desktop(frozen)
        }
        Err(error) => return Err(error),
    };
    expected.validate(&source)?;
    check_cancelled(&cancelled)?;
    let window_targets = crate::selection_assist::windows(source.bounds);
    let source_pixels = SourcePixels {
        fill: None,
        canvas: source.full(),
        planes: planes(&source, &previews)?,
        overlay: None,
    };
    let cursor = pending_cursor
        .finish()
        .map(|snapshot| crate::cursor::compose(&snapshot, &source_pixels, source.bounds))
        .transpose()?
        .flatten();
    check_cancelled(&cancelled)?;
    Ok(PreparedRefresh {
        source,
        previews,
        cursor,
        window_targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refresh_escape_requires_a_new_press_after_an_initial_hold() {
        let held = AtomicBool::new(true);
        assert!(!refresh_escape_edge(&held, true, true));
        assert!(!refresh_escape_edge(&held, false, true));
        assert!(refresh_escape_edge(&held, true, true));
        assert!(!refresh_escape_edge(&held, true, true));
    }
    #[test]
    fn refresh_modified_escape_does_not_cancel_when_only_the_modifier_is_released() {
        let held = AtomicBool::new(false);
        assert!(!refresh_escape_edge(&held, true, false));
        assert!(!refresh_escape_edge(&held, true, true));
        assert!(!refresh_escape_edge(&held, false, true));
        assert!(refresh_escape_edge(&held, true, true));
    }
    use crate::fast_capture::FrozenScreen;

    fn source(rects: &[ScreenRect], current: usize) -> (CanvasSource, Vec<Arc<RenderImage>>) {
        let screens = rects
            .iter()
            .map(|&rect| FrozenScreen {
                rect,
                preview: crate::canvas_source::render_image(
                    rect.width,
                    rect.height,
                    vec![255; rect.width as usize * rect.height as usize * 4],
                ),
            })
            .collect();
        CanvasSource::frozen_desktop(FrozenDesktop {
            screens,
            bounds: ScreenRect::union(rects).unwrap(),
            current,
            capture_ms: 0.,
        })
    }

    fn monitors() -> [ScreenRect; 2] {
        [
            ScreenRect {
                left: -3,
                top: -1,
                width: 3,
                height: 3,
            },
            ScreenRect {
                left: 1,
                top: 0,
                width: 2,
                height: 2,
            },
        ]
    }

    #[test]
    fn refresh_topology_keeps_negative_origin_and_ignores_current_pointer_monitor() {
        let (old, _) = source(&monitors(), 0);
        let (new, _) = source(&monitors(), 1);
        assert!(RefreshTopology::of(&old).matches(&new));
    }

    #[test]
    fn refresh_topology_rejects_shifted_global_origin_even_when_size_is_unchanged() {
        let rects = monitors();
        let (old, _) = source(&rects, 0);
        let shifted = rects.map(|rect| ScreenRect {
            left: rect.left + 1,
            ..rect
        });
        let (new, _) = source(&shifted, 0);
        assert!(RefreshTopology::of(&old).validate(&new).is_err());
    }

    #[test]
    fn refresh_topology_rejects_monitor_change_inside_same_virtual_bounds() {
        let rects = monitors();
        let (old, _) = source(&rects, 0);
        let mut changed = rects;
        changed[1].top -= 1;
        let (new, _) = source(&changed, 0);
        assert_eq!(old.bounds, new.bounds);
        assert!(RefreshTopology::of(&old).validate(&new).is_err());
    }

    #[test]
    fn refresh_topology_rejects_monitor_reordering() {
        let rects = monitors();
        let (old, _) = source(&rects, 0);
        let (new, _) = source(&[rects[1], rects[0]], 0);
        assert!(RefreshTopology::of(&old).validate(&new).is_err());
    }

    #[test]
    fn refresh_preview_requires_each_screen_and_its_exact_pixel_dimensions() {
        let (source, previews) = source(&monitors(), 0);
        assert!(planes(&source, &previews).is_ok());
        assert!(planes(&source, &previews[..1]).is_err());
        let mut changed = previews;
        changed[0] = crate::canvas_source::render_image(1, 1, vec![255; 4]);
        assert!(planes(&source, &changed).is_err());
    }

    #[test]
    fn refresh_frozen_source_retains_original_pixels_after_preview_vector_is_dropped() {
        let (source, previews) = source(&monitors(), 0);
        drop(previews);
        let rect = source.screen_rect(0);
        let original = source.read_region(rect).unwrap();
        assert_eq!(original.width, rect.width);
        assert_eq!(original.height, rect.height);
        assert_eq!(original.bgra, vec![255; 3 * 3 * 4]);
    }
}
