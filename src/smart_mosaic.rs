//! 智能马赛克候选的纯状态模型；真实效果区域由 Document 每次投影传入。
//! 请求结果为 ROI 内物理坐标，只在这里裁剪并转换一次全画布坐标。
pub use crate::annotation::{Bounds, Point};
pub use crate::geometry::Rect;
use std::time::{Duration, Instant};

pub const REFRESH_DELAY: Duration = Duration::from_millis(180);

/// 由宿主绑定冻结源生命周期，不使用裸指针或文档对象 ID 作为源身份。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceEpoch(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Recognition,
    /// 检测结果允许没有文本；合法几何本身即可成为候选。
    Detection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestKey {
    pub source: SourceEpoch,
    pub roi: Rect,
    pub backend: String,
    pub profile: String,
    pub kind: TaskKind,
}

impl RequestKey {
    fn valid(&self) -> bool {
        self.roi.width != 0
            && self.roi.height != 0
            && self.roi.x.checked_add(self.roi.width).is_some()
            && self.roi.y.checked_add(self.roi.height).is_some()
    }

    fn bounds(&self) -> Bounds {
        Bounds {
            x: self.roi.x as f32,
            y: self.roi.y as f32,
            width: self.roi.width as f32,
            height: self.roi.height as f32,
        }
    }
}

/// 一次真实认领的请求快照。晚回执必须同时匹配 id 与完整 key。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedRequest {
    pub id: u64,
    pub key: RequestKey,
}

/// 意图没有提交副作用。宿主仍须以真实源/文档版本准备并提交一次命令。
#[derive(Clone, Debug, PartialEq)]
pub struct RegionIntent {
    pub add: Vec<Bounds>,
    pub remove: Vec<Bounds>,
    pub next_regions: Vec<Bounds>,
}

#[derive(Clone, Debug)]
struct SuccessfulResult {
    key: RequestKey,
    // 已验证、裁剪的 ROI-local 几何；缓存不拥有 applied 状态或文本结果。
    local_regions: Vec<Bounds>,
}

#[derive(Clone, Copy, Debug)]
struct PointerGesture {
    generation: u64,
    down: Point,
    current: Point,
    candidate: Option<usize>,
    tolerance: f32,
    moved: bool,
}

#[derive(Default, Debug)]
pub struct SmartMosaic {
    active: bool,
    terminated: bool,
    key: Option<RequestKey>,
    cache: Option<SuccessfulResult>,
    owner: Option<OwnedRequest>,
    next_request_id: u64,
    pending: Option<Instant>,
    generation: u64,
    candidates: Vec<Bounds>,
    pointer: Option<PointerGesture>,
    region_tolerance: f32,
}

impl SmartMosaic {
    /// 首次显式入场可立即请求。离开后的同 key pending 不缩短、不重新计时。
    pub fn activate(&mut self, key: RequestKey, now: Instant) {
        if self.terminated || (self.active && self.key.as_ref() == Some(&key)) {
            return;
        }
        self.active = true;
        if self.key.as_ref() != Some(&key) {
            let first = self.key.is_none();
            self.bind_key(key);
            self.pending = Some(if first { now } else { now + REFRESH_DELAY });
        }
        if !self.key.as_ref().is_some_and(RequestKey::valid) {
            self.pending = None;
            return;
        }
        if self.pending.is_some() || self.owner.is_some() {
            return;
        }
        if !self.publish_cache() {
            // 失败不作为成功 empty 缓存；下一次显式入场可重新尝试。
            self.pending = Some(now);
        }
    }

    /// 仅真实 changed completion 调用；相同 key 的 no-op 不续延 deadline。
    pub fn schedule_refresh(&mut self, key: RequestKey, now: Instant) -> bool {
        if self.terminated || self.key.as_ref() == Some(&key) {
            return false;
        }
        self.bind_key(key);
        self.pending = self
            .key
            .as_ref()
            .is_some_and(RequestKey::valid)
            .then_some(now + REFRESH_DELAY);
        true
    }

    /// 切走只撤可见候选/手势；同源同 ROI 的 owned 成功可进入隐藏缓存。
    pub fn leave(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.invalidate_candidates();
    }

    pub fn terminate(&mut self) {
        self.terminated = true;
        self.active = false;
        self.key = None;
        self.cache = None;
        self.owner = None;
        self.pending = None;
        self.invalidate_candidates();
    }

    /// 不改真实 Document regions。源替换只撤旧请求、缓存、候选和手势。
    pub fn source_changed(&mut self, source: SourceEpoch, now: Instant) -> bool {
        if self.terminated || self.key.as_ref().is_none_or(|key| key.source == source) {
            return false;
        }
        if let Some(key) = &mut self.key {
            key.source = source;
        }
        self.cache = None;
        self.owner = None;
        self.invalidate_candidates();
        self.pending = (self.active && self.key.as_ref().is_some_and(RequestKey::valid))
            .then_some(now + REFRESH_DELAY);
        true
    }

    /// 宿主到 deadline 后调用；命中成功缓存时只发布候选，不启动引擎。
    pub fn begin_request(&mut self, now: Instant) -> Option<OwnedRequest> {
        if !self.active
            || self.terminated
            || self.owner.is_some()
            || self.pending.is_none_or(|deadline| now < deadline)
            || !self.key.as_ref().is_some_and(RequestKey::valid)
        {
            return None;
        }
        self.pending = None;
        if self.publish_cache() {
            return None;
        }
        let Some(id) = self.next_request_id.checked_add(1) else {
            self.terminate();
            return None;
        };
        self.next_request_id = id;
        let request = OwnedRequest {
            id,
            key: self.key.clone()?,
        };
        self.owner = Some(request.clone());
        Some(request)
    }

    pub fn complete_success(&mut self, request: &OwnedRequest, regions: Vec<Bounds>) -> bool {
        if !self.owns(request) {
            return false;
        }
        let local = Bounds {
            x: 0.,
            y: 0.,
            width: request.key.roi.width as f32,
            height: request.key.roi.height as f32,
        };
        let mut local_regions = Vec::new();
        for region in regions {
            if let Some(region) =
                intersection(region, local).filter(|region| !local_regions.contains(region))
            {
                local_regions.push(region);
            }
        }
        self.owner = None;
        self.cache = Some(SuccessfulResult {
            key: request.key.clone(),
            local_regions,
        });
        self.invalidate_candidates();
        if self.active {
            self.publish_cache();
        }
        true
    }

    /// 失败不写 empty 缓存，也不立即重试；只释放匹配的这个 owned request。
    pub fn complete_failure(&mut self, request: &OwnedRequest) -> bool {
        if !self.owns(request) {
            return false;
        }
        self.owner = None;
        self.cancel_pointer();
        true
    }

    pub fn is_active(&self) -> bool {
        self.active && !self.terminated
    }

    pub fn is_busy(&self) -> bool {
        self.owner.is_some()
    }

    pub fn request_key(&self) -> Option<&RequestKey> {
        self.key.as_ref()
    }

    pub fn pending_deadline(&self) -> Option<Instant> {
        self.pending
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 宿主将 2 DIP 转成物理像素传入；默认 0 为精确匹配，不查询系统 DPI。
    /// 显示比例改变时撤销已开始的指针手势，不能跨新的容差接续旧 Down。
    pub fn set_region_tolerance(&mut self, physical_px: f32) -> bool {
        if !physical_px.is_finite() || physical_px < 0. {
            self.cancel_pointer();
            return false;
        }
        if self.region_tolerance != physical_px {
            self.cancel_pointer();
            self.region_tolerance = physical_px;
        }
        true
    }

    pub fn region_tolerance(&self) -> f32 {
        self.region_tolerance
    }

    pub fn candidates(&self) -> &[Bounds] {
        &self.candidates
    }

    pub fn candidate_at(&self, point: Point) -> Option<usize> {
        if !finite_point(point) {
            return None;
        }
        self.candidates
            .iter()
            .rposition(|region| region.contains(point, 0.))
    }

    /// 只供 Chrome；不存入 Document、Scene 或导出内容。
    pub fn marquee(&self) -> Option<Bounds> {
        let gesture = self.pointer.as_ref()?;
        if !gesture.moved {
            return None;
        }
        intersection(
            drag_bounds(gesture.down, gesture.current),
            self.key.as_ref()?.bounds(),
        )
    }

    pub fn pointer_down(&mut self, point: Point, tolerance_px: f32) -> bool {
        // 新 Down 不能接续旧手势，即使这个 Down 被资格门拒绝。
        self.cancel_pointer();
        if !self.ready()
            || !finite_point(point)
            || !tolerance_px.is_finite()
            || tolerance_px < 0.
            || !self
                .key
                .as_ref()
                .is_some_and(|key| point_in_roi(point, key.roi))
        {
            return false;
        }
        self.pointer = Some(PointerGesture {
            generation: self.generation,
            down: point,
            current: point,
            candidate: self.candidate_at(point),
            tolerance: tolerance_px,
            moved: false,
        });
        true
    }

    pub fn pointer_move(&mut self, point: Point) {
        if !finite_point(point) {
            self.cancel_pointer();
            return;
        }
        if let Some(gesture) = &mut self.pointer {
            gesture.current = point;
            // 曾越阈值后回到 Down 也不重新变成点击。
            let distance = (point.x - gesture.down.x).hypot(point.y - gesture.down.y);
            gesture.moved |= distance > 0. && distance >= gesture.tolerance;
        }
    }

    pub fn pointer_up(&mut self, point: Point, applied: &[Bounds]) -> Option<RegionIntent> {
        self.pointer_move(point);
        let gesture = self.pointer.take()?;
        if !self.ready() || gesture.generation != self.generation {
            return None;
        }
        if !gesture.moved {
            let candidate = gesture.candidate?;
            if self.candidate_at(point) != Some(candidate) {
                return None;
            }
            let region = *self.candidates.get(candidate)?;
            return Some(toggle(region, applied, self.region_tolerance));
        }
        let marquee = intersection(
            drag_bounds(gesture.down, point),
            self.key.as_ref()?.bounds(),
        )?;
        toggle_selection(
            self.candidates
                .iter()
                .copied()
                .filter(|region| intersection(*region, marquee).is_some()),
            applied,
            self.region_tolerance,
        )
    }

    pub fn add_all(&mut self, applied: &[Bounds], held: bool) -> Option<RegionIntent> {
        if held || !self.ready() {
            return None;
        }
        self.cancel_pointer();
        additions(
            self.candidates.iter().copied(),
            applied,
            self.region_tolerance,
        )
    }

    pub fn cancel_pointer(&mut self) {
        self.pointer = None;
    }

    fn ready(&self) -> bool {
        self.is_active()
            && !self.is_busy()
            && self.pending.is_none()
            && !self.candidates.is_empty()
            && self.key.as_ref().is_some_and(RequestKey::valid)
    }

    fn owns(&self, request: &OwnedRequest) -> bool {
        !self.terminated
            && self.owner.as_ref() == Some(request)
            && self.key.as_ref() == Some(&request.key)
    }

    fn bind_key(&mut self, key: RequestKey) {
        self.key = Some(key);
        self.owner = None;
        self.invalidate_candidates();
    }

    fn invalidate_candidates(&mut self) {
        self.pointer = None;
        self.candidates.clear();
        // Pointer 被直接撤销，不允许计数溢出后借索引接续旧 Down。
        self.generation = self.generation.saturating_add(1);
    }

    fn publish_cache(&mut self) -> bool {
        let Some(key) = &self.key else {
            return false;
        };
        let Some(cache) = self.cache.as_ref().filter(|cache| cache.key == *key) else {
            return false;
        };
        if !self.active || !key.valid() {
            return false;
        }
        let offset = key.bounds();
        let candidates = cache
            .local_regions
            .iter()
            .map(|region| Bounds {
                x: region.x + offset.x,
                y: region.y + offset.y,
                width: region.width,
                height: region.height,
            })
            .collect();
        self.invalidate_candidates();
        self.candidates = candidates;
        true
    }
}

fn finite_point(point: Point) -> bool {
    point.x.is_finite() && point.y.is_finite()
}

fn valid_bounds(region: Bounds) -> bool {
    region.x.is_finite()
        && region.y.is_finite()
        && region.width.is_finite()
        && region.height.is_finite()
        && region.width > 0.
        && region.height > 0.
        && (region.x + region.width).is_finite()
        && (region.y + region.height).is_finite()
}

fn intersection(a: Bounds, b: Bounds) -> Option<Bounds> {
    if !valid_bounds(a) || !valid_bounds(b) {
        return None;
    }
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then_some(Bounds {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

fn point_in_roi(point: Point, roi: Rect) -> bool {
    point.x >= roi.x as f32
        && point.y >= roi.y as f32
        && point.x < (roi.x as f64 + roi.width as f64) as f32
        && point.y < (roi.y as f64 + roi.height as f64) as f32
}

fn drag_bounds(a: Point, b: Point) -> Bounds {
    Bounds {
        x: a.x.min(b.x),
        y: a.y.min(b.y),
        width: (a.x - b.x).abs(),
        height: (a.y - b.y).abs(),
    }
}

/// 与候选 Chrome、点击、框选和 Ctrl+A 共用同一个几何匹配合同。
/// 比较真实 Left/Top/Width/Height，而非含空隙的联合 AABB。
pub fn regions_match(candidate: Bounds, actual: Bounds, tolerance_px: f32) -> bool {
    tolerance_px.is_finite()
        && tolerance_px >= 0.
        && valid_bounds(candidate)
        && valid_bounds(actual)
        && (candidate.x - actual.x).abs() <= tolerance_px
        && (candidate.y - actual.y).abs() <= tolerance_px
        && (candidate.width - actual.width).abs() <= tolerance_px
        && (candidate.height - actual.height).abs() <= tolerance_px
}

fn additions(
    regions: impl Iterator<Item = Bounds>,
    applied: &[Bounds],
    tolerance: f32,
) -> Option<RegionIntent> {
    let mut next_regions = applied.to_vec();
    let mut add = Vec::new();
    for region in regions {
        if !next_regions
            .iter()
            .any(|actual| regions_match(region, *actual, tolerance))
        {
            next_regions.push(region);
            add.push(region);
        }
    }
    (!add.is_empty()).then_some(RegionIntent {
        add,
        remove: Vec::new(),
        next_regions,
    })
}

/// 整组已应用才取消；混合状态只补齐缺失项，删除始终使用真实文档几何。
fn toggle_selection(
    regions: impl Iterator<Item = Bounds>,
    applied: &[Bounds],
    tolerance: f32,
) -> Option<RegionIntent> {
    let selected: Vec<_> = regions.collect();
    if selected.is_empty() {
        return None;
    }
    if selected.iter().any(|region| {
        !applied
            .iter()
            .any(|actual| regions_match(*region, *actual, tolerance))
    }) {
        return additions(selected.into_iter(), applied, tolerance);
    }
    let (remove, next_regions) = applied.iter().copied().partition(|actual| {
        selected
            .iter()
            .any(|region| regions_match(*region, *actual, tolerance))
    });
    Some(RegionIntent {
        add: Vec::new(),
        remove,
        next_regions,
    })
}

fn toggle(region: Bounds, applied: &[Bounds], tolerance: f32) -> RegionIntent {
    if let Some(index) = applied
        .iter()
        .position(|actual| regions_match(region, *actual, tolerance))
    {
        let mut next_regions = applied.to_vec();
        let actual = next_regions.remove(index);
        RegionIntent {
            add: Vec::new(),
            remove: vec![actual],
            next_regions,
        }
    } else {
        let mut next_regions = applied.to_vec();
        next_regions.push(region);
        RegionIntent {
            add: vec![region],
            remove: Vec::new(),
            next_regions,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds {
        Bounds {
            x,
            y,
            width,
            height,
        }
    }

    fn point(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    fn key() -> RequestKey {
        RequestKey {
            source: SourceEpoch(1),
            roi: Rect {
                x: 100,
                y: 200,
                width: 100,
                height: 100,
            },
            backend: "winrt".into(),
            profile: "lines".into(),
            kind: TaskKind::Recognition,
        }
    }

    fn loaded() -> SmartMosaic {
        let now = Instant::now();
        let mut model = SmartMosaic::default();
        model.activate(key(), now);
        let request = model.begin_request(now).unwrap();
        assert!(model.complete_success(
            &request,
            vec![bounds(10., 10., 20., 10.), bounds(50., 50., 20., 10.)]
        ));
        model
    }

    #[test]
    fn changed_roi_waits_180ms_and_leave_preserves_pending_without_restart() {
        let now = Instant::now();
        let mut model = loaded();
        assert!(model.pointer_down(point(115., 215.), 4.));
        model.activate(key(), now);
        assert_eq!(
            model.pointer_up(point(115., 215.), &[]).unwrap().add.len(),
            1
        );
        let mut next = key();
        next.roi.x = 130;
        assert!(model.schedule_refresh(next.clone(), now));
        let generation = model.generation();
        assert!(!model.schedule_refresh(next.clone(), now + Duration::from_millis(90)));
        assert_eq!(model.generation(), generation);
        assert_eq!(model.pending_deadline(), Some(now + REFRESH_DELAY));
        model.leave();
        let hidden_generation = model.generation();
        model.leave();
        assert_eq!(model.generation(), hidden_generation);
        assert!(model.begin_request(now + REFRESH_DELAY).is_none());
        model.activate(next, now + Duration::from_millis(120));
        assert!(model
            .begin_request(now + Duration::from_millis(179))
            .is_none());
        assert!(model.candidates().is_empty());
        assert!(!model.pointer_down(point(150., 220.), 4.));
        assert!(model.begin_request(now + REFRESH_DELAY).is_some());
        assert!(model.is_busy());
    }

    #[test]
    fn stale_result_and_failure_cannot_release_new_owner_or_replace_candidates() {
        let now = Instant::now();
        let mut model = SmartMosaic::default();
        model.activate(key(), now);
        let old = model.begin_request(now).unwrap();
        let mut next = key();
        next.roi.y += 20;
        model.schedule_refresh(next, now);
        let new = model.begin_request(now + REFRESH_DELAY).unwrap();
        assert_ne!(old.id, new.id);
        assert!(!model.complete_success(&old, vec![bounds(1., 1., 5., 5.)]));
        assert!(!model.complete_failure(&old));
        let forged = OwnedRequest {
            id: new.id,
            key: old.key.clone(),
        };
        assert!(!model.complete_failure(&forged));
        assert!(model.is_busy());
        assert!(model.candidates().is_empty());
        assert!(model.complete_success(&new, vec![bounds(1., 1., 5., 5.)]));
        assert!(!model.is_busy());
        assert_eq!(model.candidates(), &[bounds(101., 221., 5., 5.)]);
        assert!(!model.complete_failure(&new));
        assert_eq!(model.candidates(), &[bounds(101., 221., 5., 5.)]);
    }

    #[test]
    fn hidden_success_reuses_raw_cache_without_restoring_pointer_or_starting_ocr() {
        let now = Instant::now();
        let mut model = SmartMosaic::default();
        model.activate(key(), now);
        let request = model.begin_request(now).unwrap();
        model.leave();
        assert!(model.is_busy());
        assert!(model.complete_success(&request, vec![bounds(10., 10., 20., 10.)]));
        assert!(!model.is_active());
        assert!(model.candidates().is_empty());
        assert!(model.marquee().is_none());
        assert!(model.add_all(&[], false).is_none());
        model.activate(key(), now);
        assert!(!model.is_busy());
        assert!(model.pending_deadline().is_none());
        assert!(model.begin_request(now).is_none());
        assert_eq!(model.candidates(), &[bounds(110., 210., 20., 10.)]);
        assert!(model.pointer_up(point(115., 215.), &[]).is_none());
    }

    #[test]
    fn successful_empty_is_cached_but_failure_retries_on_explicit_activation() {
        let now = Instant::now();
        let mut model = SmartMosaic::default();
        model.activate(key(), now);
        let failed = model.begin_request(now).unwrap();
        assert!(model.complete_failure(&failed));
        assert!(model.begin_request(now).is_none());
        model.leave();
        model.activate(key(), now);
        let success = model.begin_request(now).unwrap();
        assert!(model.complete_success(&success, Vec::new()));
        model.leave();
        model.activate(key(), now);
        assert!(model.pending_deadline().is_none());
        assert!(model.begin_request(now + REFRESH_DELAY).is_none());
        assert!(model.candidates().is_empty());
    }

    #[test]
    fn cache_key_separates_task_kind_backend_profile_and_source() {
        let now = Instant::now();
        for discriminator in 0..4 {
            let mut model = loaded();
            let mut next = key();
            match discriminator {
                0 => next.kind = TaskKind::Detection,
                1 => next.backend = "onnx".into(),
                2 => next.profile = "det-only".into(),
                _ => next.source = SourceEpoch(2),
            }
            assert!(model.schedule_refresh(next.clone(), now));
            assert!(model.candidates().is_empty());
            let request = model.begin_request(now + REFRESH_DELAY).unwrap();
            assert_eq!(request.key, next);
            assert!(model.complete_success(&request, vec![bounds(0., 0., 5., 5.)]));
            assert_eq!(model.candidates(), &[bounds(100., 200., 5., 5.)]);
        }
    }

    #[test]
    fn detection_geometry_is_clipped_offset_once_and_invalid_regions_are_rejected() {
        let now = Instant::now();
        let mut model = SmartMosaic::default();
        let mut detection = key();
        detection.kind = TaskKind::Detection;
        model.activate(detection.clone(), now);
        let request = model.begin_request(now).unwrap();
        assert!(model.complete_success(
            &request,
            vec![
                bounds(-5., -2., 15., 12.),
                bounds(90., 95., 30., 20.),
                bounds(-5., -2., 15., 12.),
                bounds(100., 0., 2., 5.),
                bounds(5., 5., 0., 1.),
                bounds(1., 1., -1., 5.),
                bounds(f32::NAN, 1., 5., 5.),
                bounds(1., 1., f32::INFINITY, 5.),
            ]
        ));
        let expected = [bounds(100., 200., 10., 10.), bounds(190., 295., 10., 5.)];
        assert_eq!(model.candidates(), &expected);
        model.leave();
        model.activate(detection, now);
        assert_eq!(model.candidates(), &expected);
        assert_eq!(model.add_all(&[], false).unwrap().next_regions, expected);
    }

    #[test]
    fn click_toggles_only_same_candidate_and_uses_current_applied_projection() {
        let mut model = loaded();
        let a = model.candidates()[0];
        let b = model.candidates()[1];
        assert!(model.pointer_down(point(115., 215.), 4.));
        let add = model.pointer_up(point(117., 215.), &[]).unwrap();
        assert_eq!(add.add, [a]);
        assert!(add.remove.is_empty());
        assert_eq!(add.next_regions, [a]);
        // Up 前真实 Document 可已改变；Down 不保存独立 applied bool。
        assert!(model.pointer_down(point(115., 215.), 4.));
        let remove = model.pointer_up(point(115., 215.), &[a, b]).unwrap();
        assert_eq!(remove.remove, [a]);
        assert_eq!(remove.next_regions, [b]);
        // 小距离但跨候选身份，不能借 Up 项执行 toggle。
        let now = Instant::now();
        let mut narrow = SmartMosaic::default();
        narrow.activate(key(), now);
        let request = narrow.begin_request(now).unwrap();
        narrow.complete_success(
            &request,
            vec![bounds(10., 10., 1., 10.), bounds(12., 10., 1., 10.)],
        );
        assert!(narrow.pointer_down(point(110.5, 215.), 4.));
        assert!(narrow.pointer_up(point(112.5, 215.), &[]).is_none());
        assert!(narrow.pointer_down(point(109., 215.), 4.));
        assert!(narrow.pointer_up(point(110.5, 215.), &[]).is_none());
    }

    #[test]
    fn reverse_marquee_fills_mixed_or_cancels_all_and_far_return_never_becomes_click() {
        let mut model = loaded();
        let a = model.candidates()[0];
        let b = model.candidates()[1];
        assert!(model.pointer_down(point(180., 275.), 4.));
        model.pointer_move(point(105., 205.));
        assert_eq!(model.marquee(), Some(bounds(105., 205., 75., 70.)));
        let intent = model.pointer_up(point(105., 205.), &[a]).unwrap();
        assert_eq!(intent.add, [b]);
        assert!(intent.remove.is_empty());
        assert_eq!(intent.next_regions, [a, b]);
        assert!(model.pointer_down(point(115., 215.), 4.));
        // 正好达到 4 物理像素也是拖拽；返回后不能借同项执行删除。
        model.pointer_move(point(119., 215.));
        model.pointer_move(point(115., 215.));
        assert!(model.pointer_up(point(115., 215.), &[a]).is_none());
        assert!(model.pointer_down(point(115., 215.), 4.));
        model.pointer_move(point(170., 270.));
        model.pointer_move(point(115., 215.));
        assert!(model.pointer_up(point(115., 215.), &[a]).is_none());
        assert!(model.pointer_down(point(180., 275.), 4.));
        let removed = model.pointer_up(point(105., 205.), &[a, b]).unwrap();
        assert!(removed.add.is_empty());
        assert_eq!(removed.remove, [a, b]);
        assert!(removed.next_regions.is_empty());
    }

    #[test]
    fn marquee_all_applied_removes_only_selected_and_can_add_them_back() {
        for (down, up) in [
            (point(105., 205.), point(180., 275.)),
            (point(180., 275.), point(105., 205.)),
        ] {
            let mut model = loaded();
            let a = model.candidates()[0];
            let b = model.candidates()[1];
            let outside = bounds(185., 285., 5., 5.);
            assert!(model.pointer_down(down, 4.));
            let removed = model.pointer_up(up, &[outside, a, b]).unwrap();
            assert!(removed.add.is_empty());
            assert_eq!(removed.remove, [a, b]);
            assert_eq!(removed.next_regions, [outside]);
            assert!(model.pointer_down(down, 4.));
            let added = model.pointer_up(up, &removed.next_regions).unwrap();
            assert_eq!(added.add, [a, b]);
            assert!(added.remove.is_empty());
            assert_eq!(added.next_regions, [outside, a, b]);
        }
    }

    #[test]
    fn marquee_empty_or_edge_only_selection_does_not_cancel_applied_regions() {
        let mut model = loaded();
        let applied = model.candidates().to_vec();
        for (down, up) in [
            (point(180., 280.), point(190., 290.)),
            (point(130., 220.), point(140., 230.)),
        ] {
            for regions in [applied.as_slice(), &[]] {
                assert!(model.pointer_down(down, 4.));
                assert!(model.pointer_up(up, regions).is_none());
            }
        }
    }

    #[test]
    fn marquee_batch_cancel_removes_duplicate_and_shared_tolerant_matches_once() {
        let now = Instant::now();
        let mut model = SmartMosaic::default();
        model.activate(key(), now);
        let request = model.begin_request(now).unwrap();
        assert!(model.complete_success(
            &request,
            vec![bounds(10., 10., 20., 10.), bounds(12., 10., 20., 10.)]
        ));
        assert!(model.set_region_tolerance(2.));
        let actual = bounds(111., 211., 21., 11.);
        let near_duplicate = bounds(111., 209., 19., 9.);
        let outside = bounds(185., 285., 5., 5.);
        for applied in [
            vec![outside, actual],
            vec![actual, outside, near_duplicate, actual],
        ] {
            assert!(model.pointer_down(point(105., 205.), 4.));
            let intent = model.pointer_up(point(140., 230.), &applied).unwrap();
            assert!(intent.add.is_empty());
            assert_eq!(intent.next_regions, [outside]);
            assert_eq!(
                intent.remove,
                applied
                    .into_iter()
                    .filter(|region| *region != outside)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn marquee_batch_toggle_commits_one_undo_for_mosaic_and_blur() {
        use crate::{
            annotation::{Annotation, Style, Tool},
            editor::Editor,
        };

        for effect in [Tool::Mosaic, Tool::Blur] {
            for keep_outside in [false, true] {
                let mut model = loaded();
                let mut editor = Editor::new(Rect::full(300, 400), Some(key().roi));
                let mut ordinary =
                    Annotation::new(Tool::Rectangle, point(5., 5.), Style::default());
                ordinary.update(point(15., 15.));
                let ordinary_id = editor.document.insert(ordinary.clone());
                editor.select_tool(Tool::SmartMosaic);
                let outside = bounds(185., 285., 5., 5.);
                let mut applied = model.candidates().to_vec();
                if keep_outside {
                    applied.push(outside);
                }
                let edit = editor
                    .prepare_smart_regions(applied.clone(), effect, editor.style)
                    .unwrap()
                    .unwrap();
                assert!(editor.commit_smart_regions(edit));
                let before = editor.document.items().to_vec();
                let revision = editor.document.edit_revision();
                assert!(model.pointer_down(point(180., 275.), 4.));
                let intent = model
                    .pointer_up(point(105., 205.), &editor.smart_applied_regions())
                    .unwrap();
                let edit = editor
                    .prepare_smart_regions(intent.next_regions, effect, editor.style)
                    .unwrap()
                    .unwrap();
                assert!(editor.commit_smart_regions(edit));
                assert_eq!(editor.document.edit_revision(), revision + 1);
                assert_eq!(editor.document.get(ordinary_id), Some(&ordinary));
                assert_eq!(
                    editor.smart_applied_regions(),
                    if keep_outside { vec![outside] } else { vec![] }
                );
                let after = editor.document.items().to_vec();
                editor.undo();
                assert_eq!(editor.document.items(), before);
                assert_eq!(editor.smart_applied_regions(), applied);
                editor.redo();
                assert_eq!(editor.document.items(), after);
            }
        }
    }

    #[test]
    fn applied_clear_new_insert_and_undo_are_external_not_a_last_id_ledger() {
        let mut model = loaded();
        let original = model.add_all(&[], false).unwrap().next_regions;
        // Clear -> Insert 新 ID；模型只见真实区域。
        assert_eq!(model.add_all(&[], false).unwrap().next_regions, original);
        assert!(model.add_all(&original, false).is_none());
        // UndoInsert -> UndoClear 原 ID/区域恢复，不再重复 apply。
        assert!(model.add_all(&original, false).is_none());
        let a = original[0];
        assert!(model.pointer_down(point(115., 215.), 4.));
        let after_delete = model.pointer_up(point(115., 215.), &original).unwrap();
        assert_eq!(after_delete.remove, [a]);
        assert!(model.add_all(&original, false).is_none());
        let restored = model.add_all(&after_delete.next_regions, false).unwrap();
        assert_eq!(restored.add, [a]);
        assert!(model.add_all(&restored.next_regions, false).is_none());
    }

    #[test]
    fn held_and_busy_ctrl_a_never_submit_and_roi_or_source_change_keeps_real_regions() {
        let now = Instant::now();
        let mut model = loaded();
        let real = model.add_all(&[], false).unwrap().next_regions;
        assert!(model.add_all(&[], true).is_none());
        assert!(model.source_changed(SourceEpoch(2), now));
        assert!(model.candidates().is_empty());
        assert!(!model.source_changed(SourceEpoch(2), now + Duration::from_millis(90)));
        assert_eq!(model.pending_deadline(), Some(now + REFRESH_DELAY));
        let request = model.begin_request(now + REFRESH_DELAY).unwrap();
        assert!(model.add_all(&[], false).is_none());
        assert!(model.complete_success(&request, vec![bounds(10., 10., 20., 10.)]));
        assert!(model.add_all(&real, false).is_none());
        let mut next = key();
        next.source = SourceEpoch(2);
        next.roi.x = 105;
        assert!(model.schedule_refresh(next, now));
        let request = model.begin_request(now + REFRESH_DELAY).unwrap();
        // 同世界位置的 ROI-local 几何变化，不再 offset 第二次、不重复 add。
        model.complete_success(&request, vec![bounds(5., 10., 20., 10.)]);
        assert!(model.add_all(&real, false).is_none());
        assert_eq!(real.len(), 2);
    }

    #[test]
    fn generation_cancel_leave_and_terminal_revoke_original_down_without_commands() {
        let now = Instant::now();
        let mut model = loaded();
        assert!(model.pointer_down(point(115., 215.), 4.));
        model.cancel_pointer();
        assert!(model.pointer_up(point(115., 215.), &[]).is_none());
        assert!(model.pointer_down(point(115., 215.), 4.));
        model.leave();
        model.activate(key(), now);
        assert!(model.pointer_up(point(115., 215.), &[]).is_none());
        assert!(model.pointer_down(point(115., 215.), 4.));
        let old_generation = model.generation();
        let mut next = key();
        next.profile = "other-lines".into();
        model.schedule_refresh(next, now);
        let request = model.begin_request(now + REFRESH_DELAY).unwrap();
        model.complete_success(&request, vec![bounds(10., 10., 20., 10.)]);
        assert!(model.generation() > old_generation);
        assert!(model.pointer_up(point(115., 215.), &[]).is_none());
        assert!(model.pointer_down(point(115., 215.), 4.));
        model.terminate();
        assert!(model.pointer_up(point(115., 215.), &[]).is_none());
        assert!(!model.complete_success(&request, vec![bounds(1., 1., 1., 1.)]));
        assert!(!model.complete_failure(&request));
        model.activate(key(), now);
        assert!(!model.is_active());
        assert!(model.begin_request(now + REFRESH_DELAY).is_none());
        assert!(model.add_all(&[], false).is_none());
    }

    #[test]
    fn region_matching_uses_explicit_physical_tolerance_and_rejects_invalid_geometry() {
        let candidate = bounds(110., 210., 20., 10.);
        assert!(regions_match(candidate, candidate, 0.));
        for dimension in 0..4 {
            let mut actual = candidate;
            match dimension {
                0 => actual.x += 2.,
                1 => actual.y += 2.,
                2 => actual.width += 2.,
                _ => actual.height += 2.,
            }
            assert!(regions_match(candidate, actual, 2.));
            assert!(!regions_match(candidate, actual, 0.));
            match dimension {
                0 => actual.x += 0.01,
                1 => actual.y += 0.01,
                2 => actual.width += 0.01,
                _ => actual.height += 0.01,
            }
            assert!(!regions_match(candidate, actual, 2.));
        }
        // 2 DIP 在不同显示比例必须由宿主明确传成 2 或 4 物理像素。
        let shifted = bounds(113., 210., 20., 10.);
        assert!(!regions_match(candidate, shifted, 2.));
        assert!(regions_match(candidate, shifted, 4.));
        for tolerance in [-1., f32::NAN, f32::INFINITY] {
            assert!(!regions_match(candidate, candidate, tolerance));
        }
        for invalid in [
            bounds(110., 210., 0., 10.),
            bounds(110., 210., 20., -1.),
            bounds(f32::NAN, 210., 20., 10.),
            bounds(110., 210., f32::INFINITY, 10.),
        ] {
            assert!(!regions_match(candidate, invalid, 4.));
            assert!(!regions_match(invalid, candidate, 4.));
        }
        let mut model = loaded();
        assert_eq!(model.region_tolerance(), 0.);
        assert!(model.pointer_down(point(115., 215.), 4.));
        assert!(model.set_region_tolerance(2.));
        assert!(model.pointer_up(point(115., 215.), &[]).is_none());
        assert!(model.pointer_down(point(115., 215.), 4.));
        assert!(model.set_region_tolerance(2.));
        assert!(model.pointer_up(point(115., 215.), &[]).is_some());
        assert!(model.pointer_down(point(115., 215.), 4.));
        assert!(!model.set_region_tolerance(f32::NAN));
        assert_eq!(model.region_tolerance(), 2.);
        assert!(model.pointer_up(point(115., 215.), &[]).is_none());
    }

    #[test]
    fn tolerant_toggle_removes_actual_geometry_and_mixed_marquee_or_all_preserve_it() {
        let mut model = loaded();
        assert!(model.set_region_tolerance(2.));
        let a = model.candidates()[0];
        let b = model.candidates()[1];
        let actual_a = bounds(a.x + 1., a.y - 1., a.width + 1., a.height + 1.);
        let outside = bounds(10., 10., 20., 10.);
        let intent = model.add_all(&[actual_a, outside], false).unwrap();
        assert_eq!(intent.add, [b]);
        assert_eq!(intent.next_regions, [actual_a, outside, b]);
        assert!(model.add_all(&intent.next_regions, false).is_none());
        assert!(model.pointer_down(point(180., 275.), 4.));
        let removed = model
            .pointer_up(point(105., 205.), &intent.next_regions)
            .unwrap();
        assert_eq!(removed.remove, [actual_a, b]);
        assert_eq!(removed.next_regions, [outside]);
        assert!(removed.add.is_empty());
        assert!(model.pointer_down(point(180., 275.), 4.));
        let mixed = model
            .pointer_up(point(105., 205.), &[actual_a, outside])
            .unwrap();
        assert_eq!(mixed.add, [b]);
        assert!(mixed.remove.is_empty());
        assert_eq!(mixed.next_regions, [actual_a, outside, b]);
        assert!(model.pointer_down(point(115., 215.), 4.));
        let removed = model
            .pointer_up(point(115., 215.), &intent.next_regions)
            .unwrap();
        assert_eq!(removed.remove, [actual_a]);
        assert_ne!(removed.remove, [a]);
        assert_eq!(removed.next_regions, [outside, b]);
        assert!(model.pointer_down(point(115., 215.), 4.));
        let last = model.pointer_up(point(115., 215.), &[actual_a]).unwrap();
        assert_eq!(last.remove, [actual_a]);
        assert!(last.next_regions.is_empty());
        // 超出显式匹配容差的真实旧区域须保留，新候选只作为 add。
        let distinct = bounds(a.x + 3., a.y, a.width, a.height);
        let added = model.add_all(&[distinct, b], false).unwrap();
        assert_eq!(added.add, [a]);
        assert_eq!(added.next_regions, [distinct, b, a]);
    }

    #[test]
    fn cached_roi_return_waits_changed_deadline_and_invalid_inputs_cannot_start_a_gesture() {
        let now = Instant::now();
        let mut model = loaded();
        let mut next = key();
        next.roi.x = 150;
        model.schedule_refresh(next, now);
        assert!(model.schedule_refresh(key(), now + Duration::from_millis(20)));
        assert!(model
            .begin_request(now + Duration::from_millis(199))
            .is_none());
        assert!(model.candidates().is_empty());
        assert!(model
            .begin_request(now + Duration::from_millis(200))
            .is_none());
        assert!(!model.candidates().is_empty());
        assert!(model.pending_deadline().is_none());
        assert!(!model.pointer_down(point(115., 215.), f32::NAN));
        assert!(!model.pointer_down(point(115., 215.), -1.));
        assert!(!model.pointer_down(point(f32::INFINITY, 215.), 4.));
        assert!(!model.pointer_down(point(200., 215.), 4.));
        assert!(model.pointer_down(point(115., 215.), 4.));
        model.pointer_move(point(f32::NAN, 215.));
        assert!(model.pointer_up(point(115., 215.), &[]).is_none());
        let mut invalid = key();
        invalid.roi.x = u32::MAX;
        model.schedule_refresh(invalid, now);
        assert!(model.begin_request(now + REFRESH_DELAY).is_none());
        assert!(!model.is_busy());
        assert!(model.candidates().is_empty());
    }
}
