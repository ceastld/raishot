//! 贴图的纯数据状态；缩放和窗口透明度不修改图片，处理历史只保存参数。
use crate::{capture::Image, Result};
#[path = "pin_processing_history.rs"]
mod processing_snapshot;
pub(crate) use processing_snapshot::WireProcessingHistory;

pub(crate) const MAX_PIN_WINDOW_SIDE: u32 = 16_384;
pub(crate) fn default_shadow_enabled() -> bool {
    true
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Adjustments {
    pub turns: u8,
    pub flip_x: bool,
    pub flip_y: bool,
    pub grayscale: bool,
    pub invert: bool,
    pub brightness: i16,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
/// 一次已确认定位对应的物理尺寸；DPI 或同尺寸内容重绘不得重新舍入两轴。
#[derive(Clone, Copy, Debug)]
pub(crate) struct PhysicalWindowGeometry {
    image: (u32, u32),
    zoom: f32,
    pub(crate) size: (u32, u32),
}
impl PhysicalWindowGeometry {
    pub(crate) fn new(image: (u32, u32), zoom: f32, size: (u32, u32)) -> Self {
        Self { image, zoom, size }
    }
    pub(crate) fn retained_size(
        self,
        image: (u32, u32),
        zoom: f32,
        explicit_user_zoom: bool,
    ) -> Option<(u32, u32)> {
        (!explicit_user_zoom && self.image == image && self.zoom == zoom).then_some(self.size)
    }
}
/// 角落辅助 UI 只在自己的区域悬浮出现，不能因鼠标进入整张图就遮挡内容。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CornerOverlayVisibility {
    pub(crate) actions: bool,
    pub(crate) info: bool,
}
impl CornerOverlayVisibility {
    pub(crate) fn at(
        pointer: (f32, f32),
        viewport: (f32, f32),
        hovered: bool,
        interacting: bool,
    ) -> Self {
        let (x, y) = pointer;
        let (width, height) = viewport;
        // 小贴图没有足够的内容避让空间，沿右键菜单和快捷键操作。
        if !hovered
            || interacting
            || ![x, y, width, height].into_iter().all(f32::is_finite)
            || width < 180.
            || height < 96.
            || x < 0.
            || y < 0.
            || x >= width
            || y >= height
        {
            return Self::default();
        }
        Self {
            actions: x >= width - 158. && y < 38.,
            info: x < 120. && y >= height - 28.,
        }
    }
}

#[cfg(test)]
mod corner_overlay_tests {
    use super::CornerOverlayVisibility as Visibility;

    #[test]
    fn corner_actions_require_their_own_hover_region() {
        let host = (800., 600.);
        assert_eq!(
            Visibility::at((400., 300.), host, true, false),
            Visibility::default()
        );
        assert_eq!(
            Visibility::at((790., 10.), host, true, false),
            Visibility {
                actions: true,
                info: false
            }
        );
        // 操作栏中的五个按钮均处于同一稳定悬浮区域。
        assert!(Visibility::at((645., 30.), host, true, false).actions);
        assert!(!Visibility::at((640., 30.), host, true, false).actions);
        assert!(!Visibility::at((790., 40.), host, true, false).actions);
    }

    #[test]
    fn zoom_and_status_only_appear_at_the_bottom_left() {
        let host = (800., 600.);
        assert_eq!(
            Visibility::at((10., 590.), host, true, false),
            Visibility {
                actions: false,
                info: true
            }
        );
        assert!(!Visibility::at((400., 590.), host, true, false).info);
        assert!(!Visibility::at((10., 560.), host, true, false).info);
    }

    #[test]
    fn editing_gestures_leaving_and_small_pins_do_not_cover_the_image() {
        for point in [(790., 10.), (10., 590.)] {
            assert_eq!(
                Visibility::at(point, (800., 600.), true, true),
                Visibility::default()
            );
            assert_eq!(
                Visibility::at(point, (800., 600.), false, false),
                Visibility::default()
            );
        }
        for host in [(24., 24.), (179., 600.), (800., 95.), (f32::NAN, 600.)] {
            assert_eq!(
                Visibility::at((10., 10.), host, true, false),
                Visibility::default()
            );
        }
        for point in [(-1., 10.), (800., 10.), (10., 600.), (f32::NAN, 10.)] {
            assert_eq!(
                Visibility::at(point, (800., 600.), true, false),
                Visibility::default()
            );
        }
    }
}

/// 左上角操作气泡比较的可见状态；百分比按显示取整，同一读数不重复弹出。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OperationHintState {
    zoom: i64,
    /// 窗口物理像素尺寸；拖边改变留白而倍率不变时也算一次缩放反馈。
    size: (u32, u32),
    opacity: i64,
    locked: bool,
    topmost: bool,
    adjustments: Adjustments,
}
impl OperationHintState {
    pub(crate) fn of(state: &PinState, size: (u32, u32)) -> Self {
        Self {
            zoom: zoom_percent(state.zoom),
            size,
            opacity: (f64::from(state.opacity) * 100. / 255.).round() as i64,
            locked: state.locked,
            topmost: state.topmost,
            adjustments: state.adjustments,
        }
    }
}

fn zoom_percent(zoom: f32) -> i64 {
    if zoom.is_finite() {
        (f64::from(zoom) * 100.).round() as i64
    } else {
        0
    }
}

/// 缩放反馈：显示倍率与窗口物理像素尺寸，如“125% · 1600×900”。
pub(crate) fn zoom_hint(zoom: f32, size: (u32, u32)) -> String {
    format!("{}% · {}×{}", zoom_percent(zoom), size.0, size.1)
}

/// 状态变化对应的一句反馈；同时变化时取最能说明本次操作的一项（旋转带来的尺寸变化不报倍率）。
pub(crate) fn operation_hint(
    before: OperationHintState,
    after: OperationHintState,
) -> Option<String> {
    if before.locked != after.locked {
        return Some(
            if after.locked {
                tr!("已锁定 · L 解锁", "Locked · L to unlock")
            } else {
                tr!("已解锁", "Unlocked")
            }
            .into(),
        );
    }
    if before.topmost != after.topmost {
        return Some(
            if after.topmost {
                tr!("已置顶", "Always on top")
            } else {
                tr!("已取消置顶", "Not on top")
            }
            .into(),
        );
    }
    if before.opacity != after.opacity {
        return Some(trf!("不透明度 {}%", "Opacity {}%", after.opacity));
    }
    let (old, new) = (before.adjustments, after.adjustments);
    if old != new {
        let changes = [
            old.turns != new.turns,
            old.flip_x != new.flip_x,
            old.flip_y != new.flip_y,
            old.grayscale != new.grayscale,
            old.invert != new.invert,
            old.brightness != new.brightness,
        ];
        let text = if new == Adjustments::default() && changes.iter().filter(|c| **c).count() > 1 {
            tr!("已还原图像", "Image reset").into()
        } else if old.turns != new.turns {
            trf!("旋转 {}°", "Rotated {}°", u32::from(new.turns) * 90)
        } else if old.flip_x != new.flip_x {
            if new.flip_x {
                tr!("水平翻转", "Flipped horizontally")
            } else {
                tr!("取消水平翻转", "Horizontal flip off")
            }
            .into()
        } else if old.flip_y != new.flip_y {
            if new.flip_y {
                tr!("垂直翻转", "Flipped vertically")
            } else {
                tr!("取消垂直翻转", "Vertical flip off")
            }
            .into()
        } else if old.grayscale != new.grayscale {
            if new.grayscale {
                tr!("灰度", "Grayscale on")
            } else {
                tr!("取消灰度", "Grayscale off")
            }
            .into()
        } else if old.invert != new.invert {
            if new.invert {
                tr!("反色", "Invert on")
            } else {
                tr!("取消反色", "Invert off")
            }
            .into()
        } else {
            let value = format!("{:+}", new.brightness);
            trf!("亮度 {}", "Brightness {}", value)
        };
        return Some(text);
    }
    (before.zoom != after.zoom || before.size != after.size)
        .then(|| format!("{}% · {}×{}", after.zoom, after.size.0, after.size.1))
}

#[cfg(test)]
mod operation_hint_tests {
    use super::{operation_hint, zoom_hint, Adjustments, OperationHintState, PinState};

    const SIZE: (u32, u32) = (800, 600);

    fn sized(before: (&PinState, (u32, u32)), after: (&PinState, (u32, u32))) -> Option<String> {
        operation_hint(
            OperationHintState::of(before.0, before.1),
            OperationHintState::of(after.0, after.1),
        )
    }

    fn hint(before: &PinState, after: &PinState) -> Option<String> {
        sized((before, SIZE), (after, SIZE))
    }

    #[test]
    fn zoom_and_opacity_report_display_percentages_once() {
        let base = PinState::default();
        let mut zoomed = base.clone();
        zoomed.zoom = 1.234;
        assert_eq!(
            sized((&base, SIZE), (&zoomed, (987, 740))).as_deref(),
            Some("123% · 987×740")
        );
        assert_eq!(zoom_hint(1.234, (987, 740)), "123% · 987×740");
        // 浮点抖动不改变显示读数时不重复弹出。
        let mut jitter = zoomed.clone();
        jitter.zoom = 1.2341;
        assert_eq!(hint(&zoomed, &jitter), None);
        // 拖边只改留白、倍率读数不变时仍报告新尺寸。
        assert_eq!(
            sized((&base, SIZE), (&base, (900, 600))).as_deref(),
            Some("100% · 900×600")
        );
        let mut faded = base.clone();
        faded.opacity = 204;
        assert!(hint(&base, &faded).is_some_and(|text| text.contains("80%")));
        assert_eq!(hint(&base, &base), None);
    }

    #[test]
    fn state_toggles_win_over_resulting_zoom_changes() {
        let base = PinState::default();
        let mut locked = base.clone();
        locked.locked = true;
        locked.zoom = 2.;
        let text = hint(&base, &locked).unwrap();
        assert!(!text.contains('%'), "{text}");
        let mut rotated = base.clone();
        rotated.adjustments = Adjustments::default().rotated(1);
        rotated.zoom = 0.5;
        // 旋转交换宽高也只报旋转。
        let text = sized((&base, SIZE), (&rotated, (600, 800))).unwrap();
        assert!(text.contains("90") && !text.contains('×'), "{text}");
        let mut brighter = base.clone();
        brighter.adjustments.brightness = 20;
        assert!(hint(&base, &brighter).unwrap().contains("+20"));
    }

    #[test]
    fn resetting_several_adjustments_reports_one_reset() {
        let mut edited = PinState::default();
        edited.adjustments.grayscale = true;
        edited.adjustments.invert = true;
        let reset = PinState::default();
        let both = hint(&edited, &reset).unwrap();
        edited.adjustments.invert = false;
        let single = hint(&edited, &reset).unwrap();
        assert_ne!(both, single);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PinState {
    pub zoom: f32,
    pub opacity: u8,
    pub locked: bool,
    pub topmost: bool,
    pub shadow_enabled: bool,
    pub thumbnail: bool,
    pub normal_zoom: f32,
    pub adjustments: Adjustments,
}
impl Default for PinState {
    fn default() -> Self {
        Self {
            zoom: 1.,
            opacity: 255,
            locked: false,
            topmost: true,
            shadow_enabled: default_shadow_enabled(),
            thumbnail: false,
            normal_zoom: 1.,
            adjustments: Adjustments::default(),
        }
    }
}
impl Adjustments {
    pub fn rotated(mut self, delta: i8) -> Self {
        self.turns = (self.turns as i8 + delta).rem_euclid(4) as u8;
        if delta.rem_euclid(2) == 1 {
            std::mem::swap(&mut self.flip_x, &mut self.flip_y);
        }
        self
    }
    pub fn dimensions(self, w: u32, h: u32) -> (u32, u32) {
        if self.turns.is_multiple_of(2) {
            (w, h)
        } else {
            (h, w)
        }
    }
    pub fn apply(self, image: &Image) -> Result<Image> {
        self.apply_bgra(image.width, image.height, &image.bgra)
    }
    /// 后台处理直接借用共享原帧，仅分配结果，避免长图先复制一份输入。
    pub(crate) fn apply_bgra(self, width: u32, height: u32, bgra: &[u8]) -> Result<Image> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or("贴图尺寸溢出")?;
        if width == 0 || height == 0 || bgra.len() != expected {
            return Err("贴图像素尺寸错误".into());
        }
        let (w, h) = self.dimensions(width, height);
        let mut bytes = vec![0; expected];
        for y in 0..height {
            for x in 0..width {
                let (mut tx, mut ty) = match self.turns % 4 {
                    1 => (height - 1 - y, x),
                    2 => (width - 1 - x, height - 1 - y),
                    3 => (y, width - 1 - x),
                    _ => (x, y),
                };
                if self.flip_x {
                    tx = w - 1 - tx;
                }
                if self.flip_y {
                    ty = h - 1 - ty;
                }
                let source = ((y * width + x) * 4) as usize;
                let dest = ((ty * w + tx) * 4) as usize;
                let p = &bgra[source..source + 4];
                let gray =
                    ((p[0] as u32 * 29 + p[1] as u32 * 150 + p[2] as u32 * 77 + 128) >> 8) as u8;
                for c in 0..3 {
                    let mut value = if self.grayscale { gray } else { p[c] } as i16;
                    if self.invert {
                        value = 255 - value;
                    }
                    bytes[dest + c] = (value + self.brightness * 255 / 100).clamp(0, 255) as u8;
                }
                bytes[dest + 3] = p[3];
            }
        }
        Ok(Image {
            width: w,
            height: h,
            bgra: bytes,
            readback_ms: 0.,
        })
    }
}
#[cfg(test)]
mod borrowed_memory_tests {
    use super::*;

    #[test]
    fn borrowed_adjustments_transform_alpha_without_changing_shared_input() {
        let pixels = [
            10, 20, 30, 0, 40, 50, 60, 80, 70, 80, 90, 150, 100, 110, 120, 200, 130, 140, 150, 230,
            160, 170, 180, 255,
        ];
        let before = pixels;
        let result = Adjustments {
            turns: 1,
            flip_x: true,
            invert: true,
            brightness: 10,
            ..Default::default()
        }
        .apply_bgra(2, 3, &pixels)
        .unwrap();
        assert_eq!((result.width, result.height), (3, 2));
        // 顺时针后水平翻转，各输出行是原图各列，由上到下。
        for (pixel, source) in result.bgra.chunks_exact(4).zip([0, 2, 4, 1, 3, 5]) {
            let original = &before[source * 4..source * 4 + 4];
            for channel in 0..3 {
                assert_eq!(
                    pixel[channel],
                    (255_i16 - i16::from(original[channel]) + 25).min(255) as u8
                );
            }
            assert_eq!(pixel[3], original[3]);
        }
        assert_eq!(pixels, before);
    }

    #[test]
    fn borrowed_adjustments_reject_invalid_pixels_before_output_allocation() {
        for (width, height, bytes) in [
            (0, 3, &[][..]),
            (2, 3, &[0; 23][..]),
            (u32::MAX, u32::MAX, &[][..]),
        ] {
            assert!(Adjustments::default()
                .apply_bgra(width, height, bytes)
                .is_err());
        }
    }
}
pub fn size(image: (u32, u32), zoom: f32) -> (u32, u32) {
    (
        (image.0 as f32 * zoom).round().clamp(24., 30000.) as u32,
        (image.1 as f32 * zoom).round().clamp(24., 30000.) as u32,
    )
}
pub fn fit(image: (u32, u32), work: Placement) -> f32 {
    (work.width.saturating_sub(32) as f32 / image.0.max(1) as f32)
        .min(work.height.saturating_sub(32) as f32 / image.1.max(1) as f32)
        .clamp(0.0001, 1.)
}
/// 长截图贴图的窗口左上角：倍率与普通贴图同样适应工作区，滚动方向在工作区居中，
/// 另一方向对齐原选区中线（未缩小时即原选区位置），再收进工作区。
/// 直接贴图与剪贴板再贴图共用，`work` 取选区所在显示器。
pub(crate) fn scroll_origin(
    image: (u32, u32),
    axis: crate::stitch::ScrollAxis,
    region: crate::geometry::ScreenRect,
    work: Placement,
) -> (i32, i32) {
    let (width, height) = size(image, fit(image, work));
    let center = |start: i32, extent: u32, span: u32| {
        (i64::from(start) + (i64::from(extent) - i64::from(span)) / 2) as i32
    };
    let inside = |value: i32, start: i32, extent: u32, span: u32| {
        value.clamp(start, start.max(start + extent as i32 - span as i32))
    };
    let (x, y) = match axis {
        crate::stitch::ScrollAxis::Vertical => (
            center(region.left, region.width, width),
            center(work.y, work.height, height),
        ),
        crate::stitch::ScrollAxis::Horizontal => (
            center(work.x, work.width, width),
            center(region.top, region.height, height),
        ),
    };
    (
        inside(x, work.x, work.width, width),
        inside(y, work.y, work.height, height),
    )
}

/// 截图的自然客户区尺寸意图不随首显、DPI 或内部图像处理结束；不写入配置格式。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PinWindowSizing {
    #[default]
    General,
    ExactCapture,
}
impl PinWindowSizing {
    pub(crate) fn size(self, image: (u32, u32), zoom: f32) -> (u32, u32) {
        match self {
            Self::General => size(image, zoom),
            Self::ExactCapture => (
                (image.0 as f32 * zoom)
                    .round()
                    .clamp(1., MAX_PIN_WINDOW_SIDE as f32) as u32,
                (image.1 as f32 * zoom)
                    .round()
                    .clamp(1., MAX_PIN_WINDOW_SIDE as f32) as u32,
            ),
        }
    }
    pub(crate) fn restored(image: (u32, u32), zoom: f32, placement: Placement) -> Self {
        if image.0 != 0
            && image.1 != 0
            && zoom.is_finite()
            && zoom > 0.
            && (placement.width < 24 || placement.height < 24)
            && (placement.width, placement.height) == Self::ExactCapture.size(image, zoom)
        {
            Self::ExactCapture
        } else {
            Self::General
        }
    }
    /// 仅在宿主已通过输入资格和定位查询后，显式缩放才结束自然尺寸意图。
    pub(crate) fn after_qualified_zoom(self, explicit_user_request: bool) -> Self {
        if explicit_user_request {
            Self::General
        } else {
            self
        }
    }
    pub(crate) fn after_user_resize(self, before: (u32, u32), after: (u32, u32)) -> Self {
        if before != after {
            Self::General
        } else {
            self
        }
    }
}

pub fn visible(mut p: Placement, work: Placement) -> Placement {
    p.x = p.x.clamp(
        work.x - (p.width as i32 - 32).max(0),
        work.x + work.width as i32 - 32,
    );
    p.y = p.y.clamp(work.y, work.y + work.height as i32 - 32);
    p
}
/// 截图原点已经包含虚拟桌面原点、紧裁剪和外观留白；不能再按工作区平移。
/// 图片编辑、剪贴板和工作区恢复继续使用普通贴图的可见性调整。
pub(crate) fn initial_placement(
    placement: Placement,
    work: Placement,
    preserve_source_origin: bool,
) -> Placement {
    if preserve_source_origin {
        placement
    } else {
        visible(placement, work)
    }
}
#[derive(Clone, Default)]
pub struct ProcessingHistory {
    undo: Vec<Adjustments>,
    redo: Vec<Adjustments>,
}
impl ProcessingHistory {
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn change(&mut self, current: &mut Adjustments, next: Adjustments) -> bool {
        if *current == next {
            return false;
        }
        self.undo.push(*current);
        if self.undo.len() > 64 {
            self.undo.remove(0);
        }
        self.redo.clear();
        *current = next;
        true
    }
    pub fn undo(&mut self, current: &mut Adjustments) -> bool {
        if let Some(old) = self.undo.pop() {
            self.redo.push(*current);
            *current = old;
            true
        } else {
            false
        }
    }
    pub fn redo(&mut self, current: &mut Adjustments) -> bool {
        if let Some(next) = self.redo.pop() {
            self.undo.push(*current);
            *current = next;
            true
        } else {
            false
        }
    }
}

pub(crate) const MAX_PROCESSING_HISTORY_STEPS: usize = 64;

/// current 和两栈均为绝对参数；不保存图片或另建处理/历史执行算法。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessingHistorySnapshot {
    pub(crate) current: Adjustments,
    pub(crate) undo: Vec<Adjustments>,
    pub(crate) redo: Vec<Adjustments>,
}
impl ProcessingHistorySnapshot {
    fn validate(&self, expected_current: Adjustments) -> Result<()> {
        for state in std::iter::once(&self.current)
            .chain(self.undo.iter())
            .chain(self.redo.iter())
        {
            if state.turns > 3 || !(-100..=100).contains(&state.brightness) {
                return Err("贴图图像处理历史包含非法旋转或亮度参数".into());
            }
        }
        if self.current != expected_current {
            return Err("贴图图像处理历史与当前参数不一致".into());
        }
        if self.undo.len().saturating_add(self.redo.len()) > MAX_PROCESSING_HISTORY_STEPS {
            return Err("贴图图像处理撤销与重做合计超过64步".into());
        }
        for stack in [&self.undo, &self.redo] {
            if stack.last() == Some(&self.current)
                || stack.windows(2).any(|pair| pair[0] == pair[1])
            {
                return Err("贴图图像处理历史包含相邻等值步骤".into());
            }
        }
        Ok(())
    }
}
impl ProcessingHistory {
    /// 取得真实两栈；坏值或超限直接报错，不裁历史、不提交用户步骤。
    pub(crate) fn bounded_snapshot(
        &self,
        current: Adjustments,
    ) -> Result<ProcessingHistorySnapshot> {
        let snapshot = ProcessingHistorySnapshot {
            current,
            undo: self.undo.clone(),
            redo: self.redo.clone(),
        };
        snapshot.validate(current)?;
        Ok(snapshot)
    }

    /// 两栈仍是原有存储顺序，后续直接由原 undo/redo 执行。
    pub(crate) fn from_bounded_snapshot(
        snapshot: ProcessingHistorySnapshot,
        expected_current: Adjustments,
    ) -> Result<Self> {
        snapshot.validate(expected_current)?;
        Ok(Self {
            undo: snapshot.undo,
            redo: snapshot.redo,
        })
    }

    /// 当前参数由宿主已校验的记录提供；所有检查完成后才替换历史。
    pub(crate) fn restore_bounded_snapshot(
        &mut self,
        expected_current: Adjustments,
        snapshot: ProcessingHistorySnapshot,
    ) -> Result<()> {
        let restored = Self::from_bounded_snapshot(snapshot, expected_current)?;
        *self = restored;
        Ok(())
    }
}

#[cfg(test)]
#[path = "pin_processing_history_tests.rs"]
mod processing_history_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scroll_origin_centers_scroll_axis_and_keeps_region_column() {
        use crate::{geometry::ScreenRect, stitch::ScrollAxis};
        let work = Placement {
            x: 0,
            y: 0,
            width: 1920,
            height: 1040,
        };
        let region = |left, top, width, height| ScreenRect {
            left,
            top,
            width,
            height,
        };
        // 未缩小：横向就是原选区位置，纵向在工作区居中。
        let column = region(400, 200, 800, 600);
        assert_eq!(
            scroll_origin((800, 900), ScrollAxis::Vertical, column, work),
            (400, 70)
        );
        // 缩小后以选区中线对齐：161 宽的窗口放在 400..1200 的中间。
        assert_eq!(
            scroll_origin((800, 5000), ScrollAxis::Vertical, column, work),
            (719, 16)
        );
        // 负坐标显示器与越过工作区右缘时收回。
        let left = Placement { x: -1920, ..work };
        assert_eq!(
            scroll_origin(
                (800, 900),
                ScrollAxis::Vertical,
                region(-1500, 0, 800, 600),
                left
            ),
            (-1500, 70)
        );
        assert_eq!(
            scroll_origin(
                (800, 900),
                ScrollAxis::Vertical,
                region(1500, 0, 800, 600),
                work
            ),
            (1120, 70)
        );
        // 横向长图：横向居中，纵向对齐选区中线。
        assert_eq!(
            scroll_origin(
                (6000, 600),
                ScrollAxis::Horizontal,
                region(100, 300, 900, 600),
                work
            ),
            (16, 505)
        );
    }
    #[test]
    fn pin_physical_geometry_keeps_rounded_window_through_same_size_processing() {
        for (image, window, expected_recomputed) in [
            ((601, 359), (240, 144), (240, 143)),
            ((600, 100), (608, 101), (606, 101)),
            ((359, 601), (144, 240), (143, 240)),
        ] {
            let zoom = crate::pin_zoom::projection(image, window).zoom;
            assert_eq!(
                PinWindowSizing::General.size(image, zoom),
                expected_recomputed
            );
            let geometry = PhysicalWindowGeometry::new(image, zoom, window);
            // 灰度、翻转、亮度及同尺寸撤销都只换内容，保留已确认的两轴尺寸。
            for _ in 0..4 {
                assert_eq!(geometry.retained_size(image, zoom, false), Some(window));
            }
            assert_eq!(geometry.size, window);
        }
    }
    #[test]
    fn pin_physical_geometry_reflows_only_for_new_image_or_requested_zoom() {
        let image = (601, 359);
        let window = (240, 144);
        let zoom = crate::pin_zoom::projection(image, window).zoom;
        let geometry = PhysicalWindowGeometry::new(image, zoom, window);
        assert_eq!(geometry.retained_size(image, zoom, true), None);
        assert_eq!(geometry.retained_size(image, 1., false), None);
        assert_eq!(geometry.retained_size((359, 601), zoom, false), None);
        assert_eq!(geometry.retained_size((240, 144), zoom, false), None);
        assert_eq!(geometry.retained_size(image, f32::NAN, false), None);
        assert_eq!(geometry.size, window);
    }
    #[test]
    fn exact_capture_sizing_survives_internal_geometry_and_ends_on_user_change() {
        let mut sizing = PinWindowSizing::ExactCapture;
        // 同尺寸处理、重复 DPI 校正与旋转均使用当前意图，不重新套用 min24。
        for image in [(22, 100), (22, 100), (100, 22), (22, 100), (1, 1)] {
            sizing = sizing.after_qualified_zoom(false);
            assert_eq!(sizing.size(image, 1.), image);
            assert_eq!(sizing.after_user_resize(image, image), sizing);
        }
        assert_eq!(sizing.after_qualified_zoom(true), PinWindowSizing::General);
        assert_eq!(
            sizing.after_user_resize((22, 100), (24, 109)),
            PinWindowSizing::General
        );
        assert_eq!(PinWindowSizing::General.size((22, 100), 1.), (24, 100));
        assert_eq!(PinWindowSizing::ExactCapture.size((23, 100), 0.5), (12, 50));
        assert_eq!(
            PinWindowSizing::ExactCapture.size((65_535, 1), 1.),
            (MAX_PIN_WINDOW_SIDE, 1)
        );
    }
    #[test]
    fn exact_capture_restore_accepts_only_matching_sub24_legacy_placement() {
        for (image, zoom, dimensions) in [
            ((22, 100), 1., (22, 100)),
            ((100, 23), 1., (100, 23)),
            ((1, 1), 1., (1, 1)),
            ((22, 100), 0.5, (11, 50)),
        ] {
            let placement = Placement {
                x: -1921,
                y: -701,
                width: dimensions.0,
                height: dimensions.1,
            };
            let restored = PinWindowSizing::restored(image, zoom, placement);
            assert_eq!(restored, PinWindowSizing::ExactCapture);
            assert_eq!(restored.size(image, zoom), dimensions);
            assert_eq!(
                PinWindowSizing::restored(
                    image,
                    zoom,
                    Placement {
                        width: placement.width + 1,
                        ..placement
                    }
                ),
                PinWindowSizing::General
            );
            assert_eq!(
                PinWindowSizing::restored(
                    image,
                    zoom,
                    Placement {
                        height: placement.height + 1,
                        ..placement
                    }
                ),
                PinWindowSizing::General
            );
        }
        let ordinary = Placement {
            x: -1,
            y: -1,
            width: 24,
            height: 100,
        };
        assert_eq!(
            PinWindowSizing::restored((22, 100), 1., ordinary),
            PinWindowSizing::General
        );
        for zoom in [0., -1., f32::NAN, f32::INFINITY] {
            assert_eq!(
                PinWindowSizing::restored(
                    (22, 100),
                    zoom,
                    Placement {
                        width: 22,
                        ..ordinary
                    }
                ),
                PinWindowSizing::General
            );
        }
        assert_eq!(
            PinWindowSizing::restored(
                (0, 1),
                1.,
                Placement {
                    width: 1,
                    height: 1,
                    ..ordinary
                }
            ),
            PinWindowSizing::General
        );
    }
    #[test]
    fn pin_rotate_after_flip_rotates_the_visible_image_clockwise() {
        let image = Image {
            width: 2,
            height: 3,
            bgra: (0..6).flat_map(|i| [i, i, i, 255]).collect(),
            readback_ms: 0.,
        };
        for turns in 0..4 {
            for flip_x in [false, true] {
                for flip_y in [false, true] {
                    let a = Adjustments {
                        turns,
                        flip_x,
                        flip_y,
                        ..Default::default()
                    };
                    let visible = a.apply(&image).unwrap();
                    let expected = Adjustments {
                        turns: 1,
                        ..Default::default()
                    }
                    .apply(&visible)
                    .unwrap();
                    assert_eq!(a.rotated(1).apply(&image).unwrap().bgra, expected.bgra);
                }
            }
        }
    }
    #[test]
    fn pin_rotations_flips_and_processing_preserve_alpha_and_source() {
        let image = Image {
            width: 2,
            height: 3,
            bgra: (0..6).flat_map(|i| [i, i + 10, i + 20, i * 40]).collect(),
            readback_ms: 0.,
        };
        let raw = image.bgra.clone();
        let a = Adjustments {
            turns: 1,
            ..Default::default()
        };
        let rotated = a.apply(&image).unwrap();
        assert_eq!((rotated.width, rotated.height), (3, 2));
        assert_eq!(
            rotated
                .bgra
                .chunks_exact(4)
                .map(|p| p[0])
                .collect::<Vec<_>>(),
            vec![4, 2, 0, 5, 3, 1]
        );
        let back = Adjustments {
            turns: 3,
            ..Default::default()
        }
        .apply(&rotated)
        .unwrap();
        assert_eq!(back.bgra, raw);
        let processed = Adjustments {
            grayscale: true,
            invert: true,
            brightness: 20,
            ..Default::default()
        }
        .apply(&image)
        .unwrap();
        for (a, b) in processed.bgra.chunks_exact(4).zip(raw.chunks_exact(4)) {
            assert_eq!(a[0], a[1]);
            assert_eq!(a[1], a[2]);
            assert_eq!(a[3], b[3]);
        }
        assert_eq!(image.bgra, raw);
        let flipped = Adjustments {
            flip_x: true,
            ..Default::default()
        }
        .apply(&image)
        .unwrap();
        assert_eq!(flipped.bgra[0], 1);
        assert_eq!(flipped.bgra[4], 0);
    }
    #[test]
    fn pin_zoom_anchor_fit_and_negative_monitor_remain_reachable() {
        let work = Placement {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1040,
        };
        let z = fit((100, 30000), work);
        assert!(z < 0.1);
        let next = size((100, 30000), z);
        assert!(next.1 <= 1040);
        let old = Placement {
            x: -1700,
            y: 200,
            width: 500,
            height: 300,
        };
        let next = crate::pin_zoom::anchored(old, (1000, 600), (1000, 600), (125., 75.));
        assert_eq!((next.x, next.y), (-1825, 125));
        assert_eq!(old.x + 125, next.x + 250);
        let v = visible(
            Placement {
                x: 5000,
                y: -800,
                ..old
            },
            work,
        );
        assert!(v.x < 0 && v.y >= 0);
    }
    #[test]
    fn pin_initial_placement_keeps_one_pixel_finish_padding_at_signed_work_top() {
        let crop = crate::geometry::Rect {
            x: 19,
            y: 23,
            width: 31,
            height: 25,
        };
        assert!(crop.valid(1000, 800));
        let layout = crate::selection_finish::SelectionFinishStyle {
            border_enabled: true,
            border_inside: false,
            border_thickness: 1.,
            ..Default::default()
        }
        .layout(crop.width, crop.height)
        .unwrap();
        assert_eq!((layout.padding.left, layout.padding.top), (1, 1));
        for source_origin in [(-1920_i32, -700_i32), (0, 0), (2560, 1440)] {
            let content = (
                source_origin.0.checked_add(crop.x as i32).unwrap(),
                source_origin.1.checked_add(crop.y as i32).unwrap(),
            );
            let work = Placement {
                x: source_origin.0,
                y: content.1,
                width: 1000,
                height: 800 - crop.y,
            };
            let initial = initial_placement(
                Placement {
                    x: content.0.checked_sub(layout.padding.left as i32).unwrap(),
                    y: content.1.checked_sub(layout.padding.top as i32).unwrap(),
                    width: layout.output_width,
                    height: layout.output_height,
                },
                work,
                true,
            );
            assert_eq!(
                (
                    initial.x.checked_add(layout.content_rect.x as i32).unwrap(),
                    initial.y.checked_add(layout.content_rect.y as i32).unwrap(),
                ),
                content
            );
            assert_eq!(initial.y, work.y - 1);
            assert_eq!((initial.width, initial.height), (33, 27));
        }
    }
    #[test]
    fn pin_initial_placement_retains_visible_policy_for_non_capture_images() {
        let work = Placement {
            x: -1920,
            y: -700,
            width: 1000,
            height: 800,
        };
        let image = Placement {
            x: -1902,
            y: -701,
            width: 33,
            height: 27,
        };
        assert_eq!(
            initial_placement(image, work, false),
            Placement { y: -700, ..image }
        );
        let far = Placement {
            x: 5000,
            y: 5000,
            ..image
        };
        assert_eq!(
            initial_placement(far, work, false),
            Placement {
                x: -952,
                y: 68,
                ..image
            }
        );
    }
    #[test]
    fn pin_initial_placement_preserves_legal_signed_limits_without_workarea_arithmetic() {
        for origin in [
            (i32::MIN + 1, i32::MIN + 1),
            (i32::MAX - 100, i32::MAX - 80),
        ] {
            let content = (origin.0, origin.1);
            let placement = Placement {
                x: content.0.checked_sub(1).unwrap(),
                y: content.1.checked_sub(1).unwrap(),
                width: 100,
                height: 80,
            };
            assert!(placement.x.checked_add(placement.width as i32).is_some());
            assert!(placement.y.checked_add(placement.height as i32).is_some());
            let actual = initial_placement(
                placement,
                Placement {
                    x: origin.0,
                    y: origin.1,
                    width: 100,
                    height: 80,
                },
                true,
            );
            assert_eq!(
                (actual.x.checked_add(1), actual.y.checked_add(1)),
                (Some(content.0), Some(content.1))
            );
            assert_eq!(actual, placement);
        }
    }
    #[test]
    fn pin_processing_history_preserves_redo_on_noop() {
        let mut h = ProcessingHistory::default();
        let mut current = Adjustments::default();
        let turned = Adjustments {
            turns: 1,
            ..current
        };
        assert!(h.change(&mut current, turned));
        assert!(h.undo(&mut current));
        let same = current;
        assert!(!h.change(&mut current, same));
        assert!(h.redo(&mut current));
        assert_eq!(current, turned);
    }
}
