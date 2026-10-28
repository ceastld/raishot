//! 后台 MTA 控件选区；COM 对象不越过线程，所有输出均为冻结画布中的物理矩形。
//!
//! 调用者须在 GPUI 交接后使用无窗口的后台线程。预算在每次 COM 调用前后检查，
//! IUIAutomation2 限制单次 provider 等待；它们不承诺强行中断正在执行的跨进程调用。
use std::{
    collections::VecDeque,
    ffi::c_void,
    marker::PhantomData,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use crate::{
    geometry::{Rect, ScreenRect},
    selection_assist::WindowTarget,
    Result,
};
use windows::{
    core::{IUnknown, Interface, HRESULT},
    Win32::{
        Foundation::{HWND, POINT, RECT},
        Graphics::Gdi::ScreenToClient,
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_MULTITHREADED, SAFEARRAY,
        },
        System::Ole::{
            SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetElemsize,
            SafeArrayGetLBound, SafeArrayGetUBound, SafeArrayGetVartype,
        },
        System::Variant::{VariantClear, VT_BOOL, VT_R8},
        UI::{
            Accessibility::{
                AutomationElementMode_Full, CUIAutomation8, IUIAutomation2,
                IUIAutomationCacheRequest, IUIAutomationElement, IUIAutomationElementArray,
                IUIAutomationTextPattern, IUIAutomationTextRange, IUIAutomationTreeWalker,
                IUIAutomationValuePattern, TextPatternRangeEndpoint_End,
                TextPatternRangeEndpoint_Start, TextUnit_Character, TextUnit_Line, TreeScope,
                TreeScope_Children, TreeScope_Element, UIA_BoundingRectanglePropertyId,
                UIA_ControlTypePropertyId, UIA_IsOffscreenPropertyId, UIA_ProcessIdPropertyId,
                UIA_SelectionItemIsSelectedPropertyId, UIA_TextPatternId, UIA_ValuePatternId,
                UIA_E_NOTSUPPORTED, UIA_PATTERN_ID,
            },
            HiDpi::{
                SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            },
            WindowsAndMessaging::{
                ChildWindowFromPointEx, GetAncestor, GetClassNameW, GetWindowLongPtrW,
                GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
                RealChildWindowFromPoint, BS_GROUPBOX, BS_TYPEMASK, CWP_SKIPINVISIBLE,
                CWP_SKIPTRANSPARENT, GA_PARENT, GA_ROOT, GWL_STYLE,
            },
        },
    },
};

const MAX_NODES: usize = 128;
const MAX_DEPTH: usize = 24;
// 冷 provider 的首次 ElementFromHandle 在真实截图覆盖层下可能超过75ms。
// 查询只在交接后的后台执行；即时窗口反馈与过期结果丢弃由调用方负责。
const PROVIDER_TIMEOUT_MS: u32 = 250;
const PROBE_BUDGET: Duration = Duration::from_millis(240);
const GEOMETRY_PROVIDER_TIMEOUT_MS: u32 = 120;
// 截图期间桌面已冻结，首批几何比重新跨进程读取更贴近冻结画面；Chrome 冷查询
// 常要 170–250ms，热缓存约 15ms。旧 250ms 租约让约一半悬停走冷路径。
const GEOMETRY_CACHE_LEASE: Duration = Duration::from_secs(30);
const MAX_GEOMETRY_NODES: usize = 4096;
const MAX_GEOMETRY_DEPTH: usize = 48;
const MAX_GEOMETRY_BATCHES: usize = 256;
const MAX_CACHED_WINDOWS: usize = 4;
const MAX_POINT_BRANCHES: usize = 12;
const MAX_WRAPPER_BRANCHES: usize = 8;
const TEXT_BUDGET: Duration = Duration::from_millis(500);
const MAX_TEXT_UNITS: usize = 16_384;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlTarget {
    pub rect: Rect,
    pub title: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlPathState {
    ControlView,
    RawView,
    /// UIA provider 无细项时使用冻结顶层窗口下的真实 Win32 子窗口几何。
    NativeChild,
    WindowOnly,
}

#[derive(Clone, Debug)]
pub struct ControlPath {
    /// Windows 全局物理坐标；targets 的 Rect 则相对 canvas 原点。
    pub point: (i32, i32),
    /// 最深命中子控件在前，冻结时缓存的顶层窗口在末尾。
    pub targets: Vec<ControlTarget>,
    pub state: ControlPathState,
    /// 普通控件/文字叶已完整核验；结构容器及截断路径不作为即时细选区。
    pub terminal: bool,
    /// 所选空间路径没有因 provider 错误、节点/深度/时间预算而截断。
    pub complete: bool,
}

/// 界面只发布最新请求代数；后台在 provider 调用边界与本地遍历中合作取消。
/// 不跨线程传 COM 引用，也不尝试终止正在执行的 provider 调用。
#[derive(Clone)]
pub(crate) struct ProbeCancellation {
    generation: Arc<AtomicU64>,
    expected: u64,
    latest_point: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
}
impl ProbeCancellation {
    pub(crate) fn new(
        generation: Arc<AtomicU64>,
        expected: u64,
        latest_point: Arc<AtomicU64>,
    ) -> Self {
        Self {
            generation,
            expected,
            latest_point,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn check_current(&self) -> Result<()> {
        if self.generation.load(Ordering::Acquire) == self.expected
            && !self.cancelled.load(Ordering::Acquire)
        {
            Ok(())
        } else {
            Err("控件查询已被最新鼠标位置取消".into())
        }
    }

    fn check_scope(&self, canvas: ScreenRect, scope: Rect) -> Result<()> {
        self.check_current()?;
        let packed = self.latest_point.load(Ordering::Acquire);
        let point = ((packed >> 32) as u32 as i32, packed as u32 as i32);
        if local_point(canvas, point).is_some_and(|point| scope.contains(point)) {
            Ok(())
        } else {
            // 观察到离开已进入的真实空间分支后不可复活为完整叶。
            self.cancelled.store(true, Ordering::Release);
            self.check_current()
        }
    }
}

pub(crate) fn pack_probe_point(point: (i32, i32)) -> u64 {
    (u64::from(point.0 as u32) << 32) | u64::from(point.1 as u32)
}

/// 兼容一次性后台探测；连续跟踪使用同一专用 MTA 线程上的 ControlProbeSession。
pub fn probe(canvas: ScreenRect, target: WindowTarget, point: (i32, i32)) -> Result<ControlPath> {
    ControlProbeSession::new()?.probe(canvas, target, point)
}

/// 只在创建它的专用后台线程使用。字段先释放 COM 引用，再关闭 apartment；
/// Rc 标记禁止 Send/Sync，元素、缓存和 DPI 状态都不能跨线程。
pub struct ControlProbeSession {
    windows: VecDeque<GeometryWindow>,
    automation: IUIAutomation2,
    control_cache: IUIAutomationCacheRequest,
    raw_cache: IUIAutomationCacheRequest,
    _com: ComScope,
    _thread: PhantomData<Rc<()>>,
}
impl ControlProbeSession {
    pub fn new() -> Result<Self> {
        let com = ComScope::new()?;
        let automation = automation_with_timeout(GEOMETRY_PROVIDER_TIMEOUT_MS)?;
        let control_cache = geometry_cache(&automation, false)?;
        let raw_cache = geometry_cache(&automation, true)?;
        Ok(Self {
            windows: VecDeque::new(),
            automation,
            control_cache,
            raw_cache,
            _com: com,
            _thread: PhantomData,
        })
    }
    pub fn probe(
        &mut self,
        canvas: ScreenRect,
        target: WindowTarget,
        point: (i32, i32),
    ) -> Result<ControlPath> {
        self.probe_with_cancellation(canvas, target, point, None)
    }
    pub(crate) fn probe_observed(
        &mut self,
        canvas: ScreenRect,
        target: WindowTarget,
        point: (i32, i32),
        cancellation: ProbeCancellation,
    ) -> Result<ControlPath> {
        self.probe_with_cancellation(canvas, target, point, Some(cancellation))
    }
    fn probe_with_cancellation(
        &mut self,
        canvas: ScreenRect,
        target: WindowTarget,
        point: (i32, i32),
        cancellation: Option<ProbeCancellation>,
    ) -> Result<ControlPath> {
        if let Some(cancellation) = &cancellation {
            cancellation.check_current()?;
        }
        let mut dpi = DpiScope::new()?;
        let answer = self.probe_physical(canvas, &target, point, cancellation.clone());
        dpi.restore()?;
        if let Some(cancellation) = &cancellation {
            cancellation.check_current()?;
        }
        answer
    }
    fn probe_physical(
        &mut self,
        canvas: ScreenRect,
        target: &WindowTarget,
        point: (i32, i32),
        cancellation: Option<ProbeCancellation>,
    ) -> Result<ControlPath> {
        validate_window(canvas, target)?;
        let local = local_point(canvas, point).ok_or("控件探测点不在物理画布内")?;
        if !target.rect.contains(local) {
            return Err("控件探测点不在原窗口内".into());
        }
        let mut budget = Budget::limited(PROBE_BUDGET, MAX_GEOMETRY_NODES);
        budget.cancellation = cancellation;
        budget.check()?;
        let native = native_child_geometry(canvas, target, point, &budget);
        if let Some(native) = &native {
            crate::uia_log::uia_log!("  native {}", branch_summary(&native.branch));
        }
        if let Some(native) = native.as_ref().filter(|native| native.branch.terminal) {
            // 原生 BUTTON/EDIT 等有精确 HWND，不等待冷 COM provider 才给出细选区。
            return native_control_path(native, canvas, target, point);
        }
        let existing = self
            .windows
            .iter()
            .position(|entry| entry.matches(canvas, target));
        let mut window = if let Some(index) = existing {
            self.windows.remove(index).expect("已找到窗口缓存")
        } else {
            // 从冻结 HWND 进入子树；ElementFromPoint 会命中自身冻结覆盖层。
            let root = checked(&budget, || unsafe {
                self.automation.ElementFromHandle(window_handle(target))
            })
            .and_then(|root| {
                validate_root(&budget, &root, target)?;
                Ok(root)
            });
            let root = match root {
                Ok(root) => root,
                Err(error) => {
                    crate::uia_log::uia_log!("  UIA 根元素失败：{error}");
                    return native.as_ref().map_or(Err(error), |native| {
                        native_control_path(native, canvas, target, point)
                    });
                }
            };
            GeometryWindow {
                canvas,
                target: target.clone(),
                root,
                lease_start: Instant::now(),
                control: GeometryTree::default(),
                raw: GeometryTree::default(),
            }
        };
        if window.lease_start.elapsed() >= GEOMETRY_CACHE_LEASE
            || window.control.near_full()
            || window.raw.near_full()
        {
            let root = checked(&budget, || unsafe {
                self.automation.ElementFromHandle(window_handle(target))
            })
            .and_then(|root| {
                validate_root(&budget, &root, target)?;
                Ok(root)
            });
            let root = match root {
                Ok(root) => root,
                Err(error) => {
                    crate::uia_log::uia_log!("  UIA 根元素失败：{error}");
                    return native.as_ref().map_or(Err(error), |native| {
                        native_control_path(native, canvas, target, point)
                    });
                }
            };
            window.root = root;
            window.control = GeometryTree::default();
            window.raw = GeometryTree::default();
            window.lease_start = Instant::now();
        }
        let seeded = window.control.batches.is_empty() && window.raw.batches.is_empty();
        let root = GeometryNode {
            element: window.root.clone(),
            bounds: target.global_bounds,
            kind: 50032,
            selected_tab: false,
            process_id: target.process_id as i32,
        };
        let mut branch = geometry_branch(
            &root,
            &self.control_cache,
            &mut window.control,
            canvas,
            target.rect,
            local,
            0,
            &mut budget,
        );
        crate::uia_log::uia_log!("  control seeded={} {}", seeded, branch_summary(&branch));
        let mut state = ControlPathState::ControlView;
        // 已解析的小叶不重复搜另一视图，结构结果尝试 RawView ownership/wrapper 细化。
        if !branch.terminal && budget.check().is_ok() {
            let raw = geometry_branch(
                &root,
                &self.raw_cache,
                &mut window.raw,
                canvas,
                target.rect,
                local,
                0,
                &mut budget,
            );
            crate::uia_log::uia_log!("  raw {}", branch_summary(&raw));
            if branch_better(&raw, &branch) {
                branch = raw;
                state = ControlPathState::RawView;
            }
        }
        if !branch.terminal {
            if let Some(native) =
                native.filter(|native| native_geometry_still_valid(native, target))
            {
                if branch_better(&native.branch, &branch) {
                    branch = native.branch.clone();
                    state = ControlPathState::NativeChild;
                }
                // 顶层 UIA provider 退化时，从真实 child HWND 的 provider 重新进入。
                if !branch.terminal && budget.check().is_ok() {
                    if let Some(entry) = native.samples.last() {
                        if let Ok(root) = checked(&budget, || unsafe {
                            self.automation
                                .ElementFromHandle(window_handle_value(entry.hwnd))
                        }) {
                            let read = (|| -> Result<GeometryNode> {
                                budget.check()?;
                                let snapshot = unsafe { root.BuildUpdatedCache(&self.raw_cache) }?;
                                budget.check_current()?;
                                let process_id = unsafe { snapshot.CachedProcessId() }?;
                                if process_id != entry.process_id as i32
                                    || process_id as u32 == std::process::id()
                                {
                                    return Err("UIA child HWND 进程身份不符".into());
                                }
                                Ok(GeometryNode {
                                    element: root,
                                    bounds: unsafe { snapshot.CachedBoundingRectangle() }?,
                                    kind: unsafe { snapshot.CachedControlType() }?.0,
                                    selected_tab: false,
                                    process_id,
                                })
                            })();
                            if let Ok(entry) = read {
                                let refined = geometry_branch(
                                    &entry,
                                    &self.raw_cache,
                                    &mut window.raw,
                                    canvas,
                                    target.rect,
                                    local,
                                    0,
                                    &mut budget,
                                );
                                crate::uia_log::uia_log!(
                                    "  child-hwnd raw {}",
                                    branch_summary(&refined)
                                );
                                if native_geometry_still_valid(&native, target)
                                    && branch_better(&refined, &branch)
                                {
                                    branch = refined;
                                    state = ControlPathState::RawView;
                                }
                            }
                        }
                    }
                }
            }
        }
        let targets = hierarchy_targets(target, branch.targets);
        if targets.len() == 1 {
            state = ControlPathState::WindowOnly;
        }
        let answer = ControlPath {
            point,
            targets,
            state,
            terminal: branch.terminal && state != ControlPathState::WindowOnly,
            complete: branch.complete,
        };
        // 缓存命中也检查冻结窗口身份与未裁切边界，移动后的旧几何不再接受。
        let validation = validate_window(canvas, target);
        if let Err(error) = &validation {
            crate::uia_log::uia_log!("  窗口校验失败：{error}");
        }
        if validation.is_ok()
            && (!window.control.batches.is_empty() || !window.raw.batches.is_empty())
        {
            if seeded {
                window.lease_start = Instant::now();
            }
            self.windows.push_front(window);
            self.windows.truncate(MAX_CACHED_WINDOWS);
        }
        validation?;
        Ok(answer)
    }
}

struct GeometryWindow {
    canvas: ScreenRect,
    target: WindowTarget,
    root: IUIAutomationElement,
    lease_start: Instant,
    control: GeometryTree,
    raw: GeometryTree,
}
impl GeometryWindow {
    fn matches(&self, canvas: ScreenRect, target: &WindowTarget) -> bool {
        self.canvas == canvas
            && self.target.hwnd == target.hwnd
            && self.target.process_id == target.process_id
            && self.target.thread_id == target.thread_id
            && self.target.rect == target.rect
            && same_native_rect(self.target.global_bounds, target.global_bounds)
    }
}
#[derive(Clone)]
struct GeometryNode {
    element: IUIAutomationElement,
    bounds: RECT,
    kind: i32,
    selected_tab: bool,
    process_id: i32,
}
#[derive(Clone)]
struct GeometryBatch {
    parent: IUIAutomationElement,
    children: Vec<GeometryNode>,
    complete: bool,
}
#[derive(Default)]
struct GeometryTree {
    batches: Vec<GeometryBatch>,
    nodes: usize,
}
impl GeometryTree {
    /// 长租约下累积的批次接近上限时整窗重建，避免单次查询中途撞上限失败。
    fn near_full(&self) -> bool {
        self.batches.len() + MAX_GEOMETRY_DEPTH >= MAX_GEOMETRY_BATCHES
            || self.nodes * 2 >= MAX_GEOMETRY_NODES
    }
}
#[derive(Clone, Default)]
struct GeometryBranch {
    // 外层到叶，只保留含点范围；非空间 wrapper 不成为可选择范围。
    targets: Vec<ControlTarget>,
    complete: bool,
    terminal: bool,
}
fn branch_summary(branch: &GeometryBranch) -> String {
    format!(
        "len={} terminal={} complete={} leaf={:?}",
        branch.targets.len(),
        branch.terminal,
        branch.complete,
        branch.targets.last().map(|target| target.rect)
    )
}
fn geometry_cache(automation: &IUIAutomation2, raw: bool) -> Result<IUIAutomationCacheRequest> {
    let cache = unsafe { automation.CreateCacheRequest() }?;
    let condition = unsafe {
        if raw {
            automation.RawViewCondition()
        } else {
            automation.ControlViewCondition()
        }
    }?;
    unsafe {
        cache.SetTreeScope(TreeScope(TreeScope_Element.0 | TreeScope_Children.0))?;
        cache.SetTreeFilter(&condition)?;
        cache.SetAutomationElementMode(AutomationElementMode_Full)?;
        for property in [
            UIA_BoundingRectanglePropertyId,
            UIA_IsOffscreenPropertyId,
            UIA_ProcessIdPropertyId,
            UIA_ControlTypePropertyId,
            UIA_SelectionItemIsSelectedPropertyId,
        ] {
            cache.AddProperty(property)?;
        }
    }
    Ok(cache)
}
fn cached_children(element: &IUIAutomationElement) -> Result<Option<IUIAutomationElementArray>> {
    let mut raw = std::ptr::null_mut();
    let result = unsafe { (element.vtable().GetCachedChildren)(element.as_raw(), &mut raw) };
    // 合法叶子可返回 S_OK + NULL；先接管引用，在错误路径也释放。
    let returned = (!raw.is_null()).then(|| unsafe { IUIAutomationElementArray::from_raw(raw) });
    result.ok()?;
    Ok(returned)
}
fn cached_selected_tab(element: &IUIAutomationElement) -> Result<bool> {
    let mut value =
        unsafe { element.GetCachedPropertyValue(UIA_SelectionItemIsSelectedPropertyId) }?;
    let selected = unsafe {
        value.Anonymous.Anonymous.vt == VT_BOOL
            && value.Anonymous.Anonymous.Anonymous.boolVal.0 != 0
    };
    unsafe { VariantClear(&mut value) }?;
    Ok(selected)
}
fn geometry_batch(
    parent: &GeometryNode,
    cache: &IUIAutomationCacheRequest,
    tree: &mut GeometryTree,
    depth: usize,
    budget: &mut Budget,
) -> Result<GeometryBatch> {
    budget.check_current()?;
    if let Some(batch) = tree
        .batches
        .iter()
        .find(|batch| batch.parent.as_raw() == parent.element.as_raw())
    {
        return Ok(batch.clone());
    }
    budget.check()?;
    if tree.batches.len() >= MAX_GEOMETRY_BATCHES || tree.nodes >= MAX_GEOMETRY_NODES {
        return Err("控件几何缓存达到上限".into());
    }
    // 一次 provider 批取父+直接孩子；不逐属性跨进程。调用后的有限快照即使
    // 超时也接管并标记 incomplete，保留已返回的安全候选。
    let snapshot = unsafe { parent.element.BuildUpdatedCache(cache) }
        .map_err(|error| uia_error("BuildUpdatedCache", &error))?;
    // 时间预算允许接管 provider 刚返回的安全快照；旧鼠标点则立即停止解析。
    budget.check_current()?;
    let observed = unsafe { snapshot.CachedBoundingRectangle() }?;
    let process_id = unsafe { snapshot.CachedProcessId() }?;
    if process_id as u32 == std::process::id() {
        return Err("UIA 几何命中自身覆盖层".into());
    }
    if process_id != parent.process_id || (depth != 0 && !same_native_rect(observed, parent.bounds))
    {
        return Err(Box::new(GeometryInvalidated));
    }
    let mut batch = GeometryBatch {
        parent: parent.element.clone(),
        children: Vec::new(),
        complete: true,
    };
    let mut fetched_count = 0;
    if let Some(children) = cached_children(&snapshot)? {
        let length = unsafe { children.Length() }?;
        if length < 0 {
            return Err("UIA 子元素数量无效".into());
        }
        let count = (length as usize).min(MAX_GEOMETRY_NODES - tree.nodes);
        fetched_count = count;
        batch.complete &= count == length as usize;
        for index in 0..count {
            budget.check_current()?;
            // 以下只读本地 cache，不因刚结束的 provider 超时丢掉整批候选。
            let read = (|| -> Result<Option<GeometryNode>> {
                let element = unsafe { children.GetElement(index as i32) }?;
                let process_id = unsafe { element.CachedProcessId() }?;
                if process_id as u32 == std::process::id() {
                    return Err("UIA 子元素命中自身覆盖层".into());
                }
                if unsafe { element.CachedIsOffscreen() }?.as_bool() {
                    return Ok(None);
                }
                let bounds = unsafe { element.CachedBoundingRectangle() }?;
                let kind = unsafe { element.CachedControlType() }?.0;
                let selected_tab = kind == 50019 && cached_selected_tab(&element)?;
                Ok(Some(GeometryNode {
                    element,
                    bounds,
                    kind,
                    selected_tab,
                    process_id,
                }))
            })();
            match read {
                Ok(Some(node)) => batch.children.push(node),
                Ok(None) => {}
                Err(_) => batch.complete = false,
            }
        }
    }
    // 本次可保留不完整批次中的安全几何；不把失败属性或截断快照带入下一次
    // provider 重试，否则短租约内重试只会反复命中同一不完整结果。
    if batch.complete {
        tree.nodes += fetched_count;
        tree.batches.push(batch.clone());
    }
    Ok(batch)
}
/// UIA SDK 普通叶控件白名单，Pane/Group/Custom/Document 不冒称细叶。
fn terminal_kind(kind: i32) -> bool {
    matches!(
        kind,
        50000
            | 50002
            | 50003
            | 50004
            | 50005
            | 50006
            | 50007
            | 50011
            | 50012
            | 50013
            | 50014
            | 50015
            | 50016
            | 50019
            | 50020
            | 50022
            | 50024
            | 50027
            | 50029
            | 50031
            | 50035
            | 50038
    )
}
fn structural_kind(kind: i32) -> bool {
    matches!(kind, 50025 | 50026 | 50030 | 50033)
}

#[derive(Debug)]
struct GeometryInvalidated;
impl std::fmt::Display for GeometryInvalidated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("控件几何或进程身份在查询期间改变")
    }
}
impl std::error::Error for GeometryInvalidated {}
fn point_geometry(
    canvas: ScreenRect,
    window: Rect,
    bounds: RECT,
    point: (i32, i32),
) -> Option<Rect> {
    clipped_rect(canvas, bounds)
        .and_then(|rect| safe_intersection(rect, window))
        .filter(|rect| rect.contains(point))
}
fn branch_better(candidate: &GeometryBranch, current: &GeometryBranch) -> bool {
    match (candidate.targets.last(), current.targets.last()) {
        (Some(_), None) => true,
        (None, _) => false,
        (Some(candidate_leaf), Some(current_leaf)) => {
            // 完整当前路径（也含容器空白）不能被失败/截断的更小BBox挤掉。
            // 同完成资格仍保持原面积、终端同面积与所有权深度的仲裁。
            if candidate.complete != current.complete {
                return candidate.complete;
            }
            let candidate_area = area(candidate_leaf.rect);
            let current_area = area(current_leaf.rect);
            candidate_area < current_area
                || (candidate_area == current_area
                    && (candidate.terminal && !current.terminal
                        || candidate.terminal == current.terminal
                            && candidate.targets.len() > current.targets.len()))
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn geometry_branch(
    node: &GeometryNode,
    cache: &IUIAutomationCacheRequest,
    tree: &mut GeometryTree,
    canvas: ScreenRect,
    window: Rect,
    point: (i32, i32),
    depth: usize,
    budget: &mut Budget,
) -> GeometryBranch {
    geometry_search(
        node,
        &mut NativeGeometrySource { cache, tree },
        canvas,
        window,
        point,
        depth,
        budget,
    )
}

#[derive(Clone, Copy)]
struct GeometryInfo {
    bounds: RECT,
    kind: i32,
    selected_tab: bool,
}
trait GeometrySource {
    type Node: Clone;
    fn info(&self, node: &Self::Node) -> GeometryInfo;
    fn children(
        &mut self,
        node: &Self::Node,
        depth: usize,
        budget: &mut Budget,
    ) -> Result<(Vec<Self::Node>, bool)>;
}
struct NativeGeometrySource<'a> {
    cache: &'a IUIAutomationCacheRequest,
    tree: &'a mut GeometryTree,
}
impl GeometrySource for NativeGeometrySource<'_> {
    type Node = GeometryNode;
    fn info(&self, node: &Self::Node) -> GeometryInfo {
        GeometryInfo {
            bounds: node.bounds,
            kind: node.kind,
            selected_tab: node.selected_tab,
        }
    }
    fn children(
        &mut self,
        node: &Self::Node,
        depth: usize,
        budget: &mut Budget,
    ) -> Result<(Vec<Self::Node>, bool)> {
        let batch = geometry_batch(node, self.cache, self.tree, depth, budget)?;
        Ok((batch.children, batch.complete))
    }
}

/// 空间仲裁与原生 COM 获取分离，合成树可验证相同生产搜索逻辑。
#[allow(clippy::too_many_arguments)]
fn geometry_search<S: GeometrySource>(
    node: &S::Node,
    source: &mut S,
    canvas: ScreenRect,
    window: Rect,
    point: (i32, i32),
    depth: usize,
    budget: &mut Budget,
) -> GeometryBranch {
    let info = source.info(node);
    let previous_scope = budget.spatial_scope;
    if !structural_kind(info.kind) && !info.selected_tab {
        if let Some(rect) = point_geometry(canvas, window, info.bounds, point) {
            budget.spatial_scope = Some((canvas, rect));
        }
    }
    let result = geometry_search_in_scope(node, source, canvas, window, point, depth, budget);
    // Provider 返回失败或空间分支结束时也观察一次最新点；兄弟恢复上级范围。
    let current = budget.check_current().is_ok();
    budget.spatial_scope = previous_scope;
    if current {
        result
    } else {
        GeometryBranch::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn geometry_search_in_scope<S: GeometrySource>(
    node: &S::Node,
    source: &mut S,
    canvas: ScreenRect,
    window: Rect,
    point: (i32, i32),
    depth: usize,
    budget: &mut Budget,
) -> GeometryBranch {
    let info = source.info(node);
    let own_rect = point_geometry(canvas, window, info.bounds, point);
    let mut result = GeometryBranch {
        targets: own_rect
            .map(|rect| ControlTarget {
                rect,
                title: String::new(),
            })
            .into_iter()
            .collect(),
        complete: false,
        terminal: false,
    };
    if depth >= MAX_GEOMETRY_DEPTH || budget.visit().is_err() {
        return result;
    }
    let (children, complete) = match source.children(node, depth, budget) {
        Ok(batch) => batch,
        Err(error) if error.is::<GeometryInvalidated>() => return GeometryBranch::default(),
        Err(_) => return result,
    };
    let mut spatial = Vec::new();
    let mut wrappers = Vec::new();
    let mut wrappers_truncated = false;
    for child in children {
        if budget.check_current().is_err() {
            return result;
        }
        let child_info = source.info(&child);
        if let Some(rect) = point_geometry(canvas, window, child_info.bounds, point) {
            spatial.push((child, rect));
        } else if structural_kind(child_info.kind) || child_info.selected_tab {
            if wrappers.len() < MAX_WRAPPER_BRANCHES {
                wrappers.push(child);
            } else {
                wrappers_truncated = true;
            }
        }
    }
    spatial.sort_by_key(|(_, rect)| area(*rect));
    let mut best = GeometryBranch::default();
    let mut scanned_complete = complete && !wrappers_truncated;
    let spatial_count = spatial.len();
    let mut inspected = 0;
    for (child, rect) in spatial.into_iter().take(MAX_POINT_BRANCHES) {
        inspected += 1;
        let branch = geometry_search(&child, source, canvas, window, point, depth + 1, budget);
        scanned_complete &= branch.complete;
        if branch_better(&branch, &best) {
            best = branch;
        }
        // 等面积 ownership 同伴会递归仲裁；确实解析出小叶才提前结束分支搜索。
        let resolved = best.terminal
            && best.complete
            && best.targets.last().is_some_and(|leaf| {
                area(leaf.rect).saturating_mul(10) <= area(rect).saturating_mul(7)
            });
        if resolved {
            break;
        }
    }
    if !best.terminal {
        for child in wrappers {
            if budget.check().is_err() {
                break;
            }
            // 不裁到 phantom bbox；最后真正含点的子叶才可被接受。
            let branch = geometry_search(&child, source, canvas, window, point, depth + 1, budget);
            scanned_complete &= branch.complete;
            if branch_better(&branch, &best) {
                best = branch;
            }
            if best.terminal {
                break;
            }
        }
    }
    if !best.targets.is_empty() {
        result.targets.extend(best.targets);
        result.complete = complete && best.complete;
        result.terminal = result.complete && best.terminal;
    } else {
        result.complete = scanned_complete && inspected == spatial_count && budget.check().is_ok();
        result.terminal = result.complete && own_rect.is_some() && terminal_kind(info.kind);
    }
    result
}

struct NativeGeometry {
    branch: GeometryBranch,
    samples: Vec<NativeSample>,
}
struct NativeSample {
    hwnd: usize,
    parent: usize,
    process_id: u32,
    thread_id: u32,
    bounds: RECT,
    class: String,
    style: i32,
}
fn native_control_path(
    native: &NativeGeometry,
    canvas: ScreenRect,
    target: &WindowTarget,
    point: (i32, i32),
) -> Result<ControlPath> {
    validate_window(canvas, target)?;
    if !native_geometry_still_valid(native, target) {
        return Err("原生子窗口在查询期间移动或被替换".into());
    }
    Ok(ControlPath {
        point,
        targets: hierarchy_targets(target, native.branch.targets.clone()),
        state: ControlPathState::NativeChild,
        terminal: native.branch.terminal,
        complete: native.branch.complete,
    })
}
fn window_handle_value(hwnd: usize) -> HWND {
    HWND(hwnd as *mut c_void)
}
fn native_terminal_class(class: &str, style: i32) -> bool {
    if class.eq_ignore_ascii_case("Button") && style & BS_TYPEMASK == BS_GROUPBOX {
        return false;
    }
    ["Button", "Edit", "Static", "ScrollBar"]
        .iter()
        .any(|leaf| class.eq_ignore_ascii_case(leaf))
}
fn native_sample_valid(sample: &NativeSample, target: &WindowTarget) -> bool {
    let hwnd = window_handle_value(sample.hwnd);
    let mut process_id = 0;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    let mut bounds = RECT::default();
    unsafe {
        IsWindow(Some(hwnd)).as_bool()
            && IsWindowVisible(hwnd).as_bool()
            && process_id != std::process::id()
            && process_id == sample.process_id
            && thread_id == sample.thread_id
            && GetAncestor(hwnd, GA_ROOT) == window_handle(target)
            && GetAncestor(hwnd, GA_PARENT) == window_handle_value(sample.parent)
            && GetWindowLongPtrW(hwnd, GWL_STYLE) as i32 == sample.style
            && native_window_class(hwnd) == sample.class
            && GetWindowRect(hwnd, &mut bounds).is_ok()
            && same_native_rect(bounds, sample.bounds)
    }
}
fn native_window_class(hwnd: HWND) -> String {
    let mut class = [0_u16; 256];
    let length = unsafe { GetClassNameW(hwnd, &mut class) }.max(0) as usize;
    String::from_utf16_lossy(&class[..length.min(class.len())])
}
fn native_geometry_still_valid(native: &NativeGeometry, target: &WindowTarget) -> bool {
    !native.samples.is_empty()
        && native
            .samples
            .iter()
            .all(|sample| native_sample_valid(sample, target))
}
fn native_child_geometry(
    canvas: ScreenRect,
    target: &WindowTarget,
    point: (i32, i32),
    budget: &Budget,
) -> Option<NativeGeometry> {
    let local = local_point(canvas, point)?;
    let mut current = window_handle(target);
    let mut samples = Vec::new();
    let mut targets = Vec::new();
    let mut complete = false;
    for _ in 0..32 {
        if budget.check().is_err() {
            break;
        }
        let mut client = POINT {
            x: point.0,
            y: point.1,
        };
        if !unsafe { ScreenToClient(current, &mut client) }.as_bool() {
            break;
        }
        // 只查定点的直接孩子，按 Win32 Z 序逐层深入，不遍历整个窗口子树。
        let mut child = unsafe {
            ChildWindowFromPointEx(current, client, CWP_SKIPINVISIBLE | CWP_SKIPTRANSPARENT)
        };
        if child == current {
            complete = true;
            break;
        }
        if child.0.is_null() {
            break;
        }
        let mut class = native_window_class(child);
        let mut style = unsafe { GetWindowLongPtrW(child, GWL_STYLE) } as i32;
        if class.eq_ignore_ascii_case("Button") && style & BS_TYPEMASK == BS_GROUPBOX {
            // GroupBox bbox 常覆盖同父层 Edit/Button；专用命中函数可找到后面的孩子。
            let behind = unsafe { RealChildWindowFromPoint(current, client) };
            if behind != child && behind != current && !behind.0.is_null() {
                child = behind;
                class = native_window_class(child);
                style = unsafe { GetWindowLongPtrW(child, GWL_STYLE) } as i32;
            }
        }
        let mut process_id = 0;
        let thread_id = unsafe { GetWindowThreadProcessId(child, Some(&mut process_id)) };
        let mut bounds = RECT::default();
        unsafe { GetWindowRect(child, &mut bounds) }.ok()?;
        let rect = point_geometry(canvas, target.rect, bounds, local)?;
        let sample = NativeSample {
            hwnd: child.0 as usize,
            parent: current.0 as usize,
            process_id,
            thread_id,
            bounds,
            class,
            style,
        };
        if process_id == 0 || thread_id == 0 || !native_sample_valid(&sample, target) {
            return None;
        }
        samples.push(sample);
        targets.push(ControlTarget {
            rect,
            title: String::new(),
        });
        current = child;
    }
    let terminal = complete
        && samples
            .last()
            .is_some_and(|leaf| native_terminal_class(&leaf.class, leaf.style));
    let native = NativeGeometry {
        branch: GeometryBranch {
            targets,
            complete,
            terminal,
        },
        samples,
    };
    native_geometry_still_valid(&native, target).then_some(native)
}

/// 只读原始 BBox 精确等于选区的一个叶控件正文；其他区域返回 None，供 OCR 回退。
/// 不使用 CurrentName 冒充正文，不读取密码控件，不聚合多个 ROI 或重复容器文本。
pub fn text(canvas: ScreenRect, targets: Vec<WindowTarget>, rect: Rect) -> Result<Option<String>> {
    let mut dpi = DpiScope::new()?;
    let answer = text_mta(canvas, &targets, rect);
    dpi.restore()?;
    answer
}

fn text_mta(canvas: ScreenRect, targets: &[WindowTarget], rect: Rect) -> Result<Option<String>> {
    if !rect.valid(canvas.width, canvas.height) {
        return Err("文字选区超出物理画布".into());
    }
    // 冻结 Z 序中任何上层窗口遮挡 ROI 都交给 OCR，不能读出被遮挡的下层正文。
    let Some(target) = text_window_target(targets, rect) else {
        return Ok(None);
    };
    let _com = ComScope::new()?;
    let mut budget = Budget::new(TEXT_BUDGET);
    validate_window(canvas, target)?;
    let point = global_center(canvas, rect).ok_or("文字选区坐标溢出")?;
    let automation = automation()?;
    let root = checked(&budget, || unsafe {
        automation.ElementFromHandle(window_handle(target))
    })?;
    validate_root(&budget, &root, target)?;
    let (mut nodes, _) = path_nodes(&automation, &root, canvas, target, point, &mut budget)?;
    let root_rect = checked(&budget, || unsafe { root.CurrentBoundingRectangle() })?;
    let root_password = checked(&budget, || unsafe { root.CurrentIsPassword() })?.as_bool();
    nodes.insert(
        0,
        Node {
            element: root,
            raw_rect: root_rect,
            rect: target.rect,
            password: root_password,
        },
    );
    let mut answer = None;
    let raw_walker = checked(&budget, || unsafe { automation.RawViewWalker() })?;
    // 原始（未 clip）BBox 须精确覆盖 ROI。纯 String 不能证明混合区域剩余部分已处理。
    for node in nodes.iter().rev() {
        let Some(full_rect) = uncut_local_rect(canvas, node.raw_rect) else {
            continue;
        };
        if !text_covers_selection(rect, full_rect) {
            continue;
        }
        // 祖先任意一个密码标记都拒绝，包括 provider 只在外层标注密码的情形。
        if nodes.iter().any(|ancestor| ancestor.password) {
            break;
        }
        // 有任何 RawView 子元素的容器不冒称全文覆盖，包括 ControlView 隐藏的子元素。
        if relative_element(&raw_walker, &node.element, true, &budget)?.is_some() {
            continue;
        }
        if let Some(value) = element_text(&budget, &node.element)? {
            let current_rect = checked(&budget, || unsafe {
                node.element.CurrentBoundingRectangle()
            })?;
            if !same_native_rect(current_rect, node.raw_rect) {
                return Err("文字控件在读取期间移动".into());
            }
            answer = Some(value);
            break;
        }
    }
    budget.check()?;
    validate_window(canvas, target)?;
    Ok(answer)
}

/// 部分文字证据；rect 相对冻结 canvas 的物理原点，绝不声明覆盖整个 ROI。
/// TextPattern 块是经完整可见性核验的行；ValuePattern 块保留完整控件范围。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextBlock {
    pub rect: Rect,
    pub text: String,
}

const MAX_REGION_LINES: usize = 128;
const MAX_LINE_UNITS: usize = 2048;
const MAX_RECTANGLE_VALUES: usize = 512;

/// 后台 MTA 提取有界区域内的完整文字块；COM 范围和 SAFEARRAY 只在调用线程存活。
/// 超时、树不完整或 provider 局部失败时保留已核验的块，其余位置仍须 OCR。
/// 不读取 Name，不将当前 UIA 文字当作已与冻结像素一致的证明。
pub fn text_blocks(
    canvas: ScreenRect,
    targets: Vec<WindowTarget>,
    rect: Rect,
) -> Result<Vec<TextBlock>> {
    let mut dpi = DpiScope::new()?;
    let answer = text_blocks_mta(canvas, &targets, rect);
    dpi.restore()?;
    answer
}

fn text_blocks_mta(
    canvas: ScreenRect,
    targets: &[WindowTarget],
    rect: Rect,
) -> Result<Vec<TextBlock>> {
    if !rect.valid(canvas.width, canvas.height) {
        return Err("文字选区超出物理画布".into());
    }
    let _com = ComScope::new()?;
    let mut budget = Budget::new(TEXT_BUDGET);
    let automation = automation()?;
    let walker = checked(&budget, || unsafe { automation.RawViewWalker() })?;
    let mut output = RegionOutput::default();
    for (index, target) in targets.iter().enumerate() {
        if budget.check().is_err() || output.full() {
            break;
        }
        let Some(clip) = safe_intersection(rect, target.rect) else {
            continue;
        };
        if validate_window(canvas, target).is_err() {
            continue;
        }
        let start = output.blocks.len();
        let units = output.units;
        let root = checked(&budget, || unsafe {
            automation.ElementFromHandle(window_handle(target))
        });
        if let Ok(root) = root {
            if validate_root(&budget, &root, target).is_ok() {
                let context = RegionContext {
                    canvas,
                    targets,
                    window_index: index,
                    clip,
                };
                let _ = walk_region(
                    &walker,
                    &root,
                    &context,
                    clip,
                    0,
                    &mut Vec::new(),
                    &mut budget,
                    &mut output,
                );
            }
        }
        // 整个原窗口失效时，该窗口此前完成的块也不能对应冻结位置。
        if validate_window(canvas, target).is_err() {
            output.blocks.truncate(start);
            output.units = units;
        }
    }
    Ok(output.blocks)
}

struct RegionContext<'a> {
    canvas: ScreenRect,
    targets: &'a [WindowTarget],
    window_index: usize,
    clip: Rect,
}

#[derive(Clone, Copy, Default)]
struct SubtreeRead {
    complete: bool,
    protected: bool,
    invalidated: bool,
}
impl SubtreeRead {
    fn may_read_container(self) -> bool {
        self.complete && !self.protected && !self.invalidated
    }
    fn include(&mut self, child: Self) {
        self.complete &= child.complete;
        self.protected |= child.protected;
        self.invalidated |= child.invalidated;
    }
}

#[derive(Default)]
struct RegionOutput {
    blocks: Vec<TextBlock>,
    units: usize,
    lines: usize,
}
impl RegionOutput {
    fn rollback(&mut self, start: usize, units: usize) {
        self.blocks.truncate(start);
        self.units = units;
    }
    fn full(&self) -> bool {
        self.blocks.len() >= MAX_REGION_LINES
            || self.lines >= MAX_REGION_LINES
            || self.units >= MAX_TEXT_UNITS
    }
    fn add(&mut self, rect: Rect, text: &str) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return true;
        }
        if self
            .blocks
            .iter()
            .any(|block| block.rect == rect && block.text == text)
        {
            return true;
        }
        let units = text.encode_utf16().count();
        if self.blocks.len() >= MAX_REGION_LINES
            || self
                .units
                .checked_add(units)
                .is_none_or(|sum| sum > MAX_TEXT_UNITS)
        {
            return false;
        }
        self.blocks.push(TextBlock {
            rect,
            text: text.into(),
        });
        self.units += units;
        true
    }
}

// 子树先审计，再读取容器的聚合 TextPattern；未知或密码孩子不允许父正文绕过保护。
#[allow(clippy::too_many_arguments)]
fn walk_region(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
    context: &RegionContext<'_>,
    parent_clip: Rect,
    depth: usize,
    ancestors: &mut Vec<(IUIAutomationElement, RECT)>,
    budget: &mut Budget,
    output: &mut RegionOutput,
) -> SubtreeRead {
    let unknown = SubtreeRead {
        complete: false,
        protected: true,
        invalidated: false,
    };
    let subtree_start = output.blocks.len();
    let subtree_units = output.units;
    if budget.visit().is_err() || output.full() {
        return unknown;
    }
    let metadata = (|| -> Result<_> {
        let password = checked(budget, || unsafe { element.CurrentIsPassword() })?.as_bool();
        if password {
            return Ok(None);
        }
        let pid = checked(budget, || unsafe { element.CurrentProcessId() })?;
        if pid <= 0 || pid as u32 != context.targets[context.window_index].process_id {
            return Err("区域文字子元素不属于冻结窗口进程".into());
        }
        let raw = checked(budget, || unsafe { element.CurrentBoundingRectangle() })?;
        let offscreen = checked(budget, || unsafe { element.CurrentIsOffscreen() })?.as_bool();
        Ok(Some((raw, offscreen)))
    })();
    let (raw, offscreen) = match metadata {
        Ok(Some(value)) => value,
        Ok(None) => {
            return SubtreeRead {
                complete: true,
                protected: true,
                invalidated: false,
            }
        }
        Err(_) => return unknown,
    };
    // 零尺寸或 offscreen 包装层仍可拥有可见孩子；可见祖先则保留其 viewport 约束。
    let clip = if !offscreen && raw.right > raw.left && raw.bottom > raw.top {
        let Some(clip) = clipped_rect(context.canvas, raw)
            .and_then(|bounds| safe_intersection(parent_clip, bounds))
        else {
            return SubtreeRead {
                complete: false,
                protected: false,
                invalidated: false,
            };
        };
        clip
    } else {
        parent_clip
    };
    ancestors.push((element.clone(), raw));
    let mut state = SubtreeRead {
        complete: true,
        protected: false,
        invalidated: false,
    };
    let mut child = match relative_element(walker, element, true, budget) {
        Ok(child) => child,
        Err(_) => {
            state.include(unknown);
            None
        }
    };
    let has_children = child.is_some();
    if depth >= MAX_DEPTH && child.is_some() {
        state.include(unknown);
        child = None;
    }
    while let Some(current) = child {
        state.include(walk_region(
            walker,
            &current,
            context,
            clip,
            depth + 1,
            ancestors,
            budget,
            output,
        ));
        if budget.check().is_err() || output.full() {
            state.include(unknown);
            break;
        }
        child = match relative_element(walker, &current, false, budget) {
            Ok(child) => child,
            Err(_) => {
                state.include(unknown);
                None
            }
        };
    }
    let node_start = output.blocks.len();
    let node_units = output.units;
    let answer = if state.may_read_container() && !offscreen && !output.full() {
        capture_region_node(
            element,
            raw,
            has_children,
            context,
            clip,
            ancestors,
            budget,
            output,
        )
    } else {
        Ok(())
    };
    let verification = validate_region_ancestors(context, ancestors, budget);
    let changed = state.invalidated
        || answer
            .as_ref()
            .err()
            .is_some_and(|error| error.is::<RegionAncestorChanged>())
        || verification
            .as_ref()
            .err()
            .is_some_and(|error| error.is::<RegionAncestorChanged>());
    if changed {
        // 明确变化的祖先下，先前成功的孩子也失去原位置/保护资格，不按普通partial保留。
        output.rollback(subtree_start, subtree_units);
        state.invalidated = true;
        state.include(unknown);
    } else if answer.is_err() || verification.is_err() {
        // 未知/预算失败仅撤销本节点未完成正文，先前各自复验完成的孩子仍是partial证据。
        output.rollback(node_start, node_units);
        state.include(unknown);
    }
    ancestors.pop();
    state
}

#[derive(Debug)]
struct RegionAncestorChanged;
impl std::fmt::Display for RegionAncestorChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("文字块祖先身份、保护属性或位置已改变")
    }
}
impl std::error::Error for RegionAncestorChanged {}

fn region_ancestor_observation(budget: &Budget, changed: bool) -> Result<()> {
    // 已返回的变化证据优先于调用后的超时；不能降级为可保留子树的未知失败。
    if changed {
        return Err(Box::new(RegionAncestorChanged));
    }
    budget.check()
}

fn validate_region_ancestors(
    context: &RegionContext<'_>,
    ancestors: &[(IUIAutomationElement, RECT)],
    budget: &Budget,
) -> Result<()> {
    for (element, raw) in ancestors {
        budget.check()?;
        let password = unsafe { element.CurrentIsPassword() }?.as_bool();
        region_ancestor_observation(budget, password)?;
        let pid = unsafe { element.CurrentProcessId() }?;
        region_ancestor_observation(
            budget,
            pid <= 0 || pid as u32 != context.targets[context.window_index].process_id,
        )?;
        let now = unsafe { element.CurrentBoundingRectangle() }?;
        region_ancestor_observation(budget, !same_native_rect(*raw, now))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn capture_region_node(
    element: &IUIAutomationElement,
    raw: RECT,
    has_children: bool,
    context: &RegionContext<'_>,
    clip: Rect,
    ancestors: &[(IUIAutomationElement, RECT)],
    budget: &Budget,
    output: &mut RegionOutput,
) -> Result<()> {
    validate_region_ancestors(context, ancestors, budget)?;
    if let Some(pattern) =
        current_pattern::<IUIAutomationTextPattern>(budget, element, UIA_TextPatternId)?
    {
        return capture_region_lines(&pattern, context, clip, budget, output);
    }
    // Value 没有逻辑行边界；只作为完整控件块，不按换行伪造每行的位置。
    if has_children {
        return Ok(());
    }
    let Some(bounds) = uncut_local_rect(context.canvas, raw) else {
        return Ok(());
    };
    if !contains_rect(clip, bounds) || !region_block_visible(context, bounds) {
        return Ok(());
    }
    if let Some(pattern) =
        current_pattern::<IUIAutomationValuePattern>(budget, element, UIA_ValuePatternId)?
    {
        let value = checked(budget, || unsafe { pattern.CurrentValue() })?.to_string();
        if let Some(value) = bounded_text(value)? {
            if !output.add(bounds, &value) {
                return Err("区域正文达到总量上限".into());
            }
        }
    }
    Ok(())
}

fn region_block_visible(context: &RegionContext<'_>, block: Rect) -> bool {
    contains_rect(context.clip, block)
        && contains_rect(context.targets[context.window_index].rect, block)
        && !context.targets[..context.window_index]
            .iter()
            .any(|above| safe_intersection(above.rect, block).is_some())
}

fn capture_region_lines(
    pattern: &IUIAutomationTextPattern,
    context: &RegionContext<'_>,
    clip: Rect,
    budget: &Budget,
    output: &mut RegionOutput,
) -> Result<()> {
    let mut anchors = Vec::new();
    for point in region_anchor_points(context.canvas, clip)? {
        let anchor = checked(budget, || unsafe { pattern.RangeFromPoint(point) })?;
        checked(budget, || unsafe {
            anchor.ExpandToEnclosingUnit(TextUnit_Line)
        })?;
        let coordinates = range_coordinates(budget, &anchor)?;
        if has_region_anchor(context.canvas, clip, &coordinates) {
            anchors.push(anchor);
        }
    }
    let Some(mut first) = anchors.first().cloned() else {
        return Ok(());
    };
    let mut last = first.clone();
    for anchor in &anchors {
        if checked(budget, || unsafe {
            anchor.CompareEndpoints(
                TextPatternRangeEndpoint_Start,
                &first,
                TextPatternRangeEndpoint_Start,
            )
        })? < 0
        {
            first = anchor.clone();
        }
        if checked(budget, || unsafe {
            anchor.CompareEndpoints(
                TextPatternRangeEndpoint_End,
                &last,
                TextPatternRangeEndpoint_End,
            )
        })? > 0
        {
            last = anchor.clone();
        }
    }
    let cursor = checked(budget, || unsafe { first.Clone() })?;
    while !output.full() {
        if checked(budget, || unsafe {
            cursor.CompareEndpoints(
                TextPatternRangeEndpoint_Start,
                &last,
                TextPatternRangeEndpoint_End,
            )
        })? >= 0
        {
            break;
        }
        output.lines += 1;
        let coordinates = range_coordinates(budget, &cursor)?;
        if let Some(bounds) = region_line_bounds(context.canvas, clip, &coordinates) {
            if region_block_visible(context, bounds) {
                let value = checked(budget, || unsafe {
                    cursor.GetText((MAX_LINE_UNITS + 1) as i32)
                })?
                .to_string();
                if region_line_text(&value).is_some()
                    && entire_region_line_visible(
                        pattern,
                        &cursor,
                        &value,
                        context.canvas,
                        clip,
                        bounds,
                        budget,
                    )?
                    && !output.add(bounds, &value)
                {
                    return Err("区域正文达到总量上限".into());
                }
            }
        }
        let previous = checked(budget, || unsafe { cursor.Clone() })?;
        if checked(budget, || unsafe { cursor.Move(TextUnit_Line, 1) })? == 0 {
            break;
        }
        if checked(budget, || unsafe {
            cursor.CompareEndpoints(
                TextPatternRangeEndpoint_Start,
                &previous,
                TextPatternRangeEndpoint_Start,
            )
        })? <= 0
        {
            return Err("区域正文 provider 行游标未前进".into());
        }
    }
    Ok(())
}

fn region_line_text(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()
        && value.encode_utf16().count() <= MAX_LINE_UNITS
        && !trimmed.contains(['\r', '\n', '\u{2028}', '\u{2029}']))
    .then_some(trimmed)
}

#[allow(clippy::too_many_arguments)]
fn entire_region_line_visible(
    pattern: &IUIAutomationTextPattern,
    line: &IUIAutomationTextRange,
    value: &str,
    canvas: ScreenRect,
    clip: Rect,
    bounds: Rect,
    budget: &Budget,
) -> Result<bool> {
    let content = checked(budget, || unsafe { line.Clone() })?;
    let expected = value.trim_end_matches(['\r', '\n']);
    let mut current = value.to_owned();
    for _ in 0..2 {
        if !current.ends_with(['\r', '\n']) {
            break;
        }
        if checked(budget, || unsafe {
            content.MoveEndpointByUnit(TextPatternRangeEndpoint_End, TextUnit_Character, -1)
        })? != -1
        {
            return Ok(false);
        }
        current = checked(budget, || unsafe {
            content.GetText((MAX_LINE_UNITS + 1) as i32)
        })?
        .to_string();
    }
    if current != expected {
        return Ok(false);
    }
    let y = i64::from(canvas.top) + i64::from(bounds.y) + i64::from(bounds.height / 2);
    let left = i64::from(canvas.left) + i64::from(clip.x);
    let right = left + i64::from(clip.width) - 1;
    let left_point = POINT {
        x: i32::try_from(left)?,
        y: i32::try_from(y)?,
    };
    let right_point = POINT {
        x: i32::try_from(right)?,
        y: i32::try_from(y)?,
    };
    let left = checked(budget, || unsafe { pattern.RangeFromPoint(left_point) })?;
    let right = checked(budget, || unsafe { pattern.RangeFromPoint(right_point) })?;
    // BBox 会被滚动 viewport 裁切；还须两端最近字符覆盖完整逻辑行，拒绝隐藏前后缀。
    Ok(checked(budget, || unsafe {
        left.CompareEndpoints(
            TextPatternRangeEndpoint_Start,
            &content,
            TextPatternRangeEndpoint_Start,
        )
    })? <= 0
        && checked(budget, || unsafe {
            right.CompareEndpoints(
                TextPatternRangeEndpoint_Start,
                &content,
                TextPatternRangeEndpoint_End,
            )
        })? >= 0)
}

fn region_anchor_points(canvas: ScreenRect, clip: Rect) -> Result<[POINT; 4]> {
    let left = i64::from(canvas.left) + i64::from(clip.x);
    let top = i64::from(canvas.top) + i64::from(clip.y);
    let right = left + i64::from(clip.width) - 1;
    let bottom = top + i64::from(clip.height) - 1;
    let inset = i64::from((clip.height / 4).min(8));
    let xs = [
        left + i64::from(clip.width / 5),
        right - i64::from(clip.width / 5),
    ];
    let ys = [(top + inset).min(bottom), (bottom - inset).max(top)];
    let point = |x, y| -> Result<POINT> {
        Ok(POINT {
            x: i32::try_from(x)?,
            y: i32::try_from(y)?,
        })
    };
    Ok([
        point(xs[0], ys[0])?,
        point(xs[1], ys[0])?,
        point(xs[0], ys[1])?,
        point(xs[1], ys[1])?,
    ])
}

fn region_coordinate_rect(canvas: ScreenRect, values: &[f64]) -> Option<Rect> {
    if values.len() != 4
        || values.iter().any(|value| !value.is_finite())
        || values[2] <= 1.0
        || values[3] <= 1.0
    {
        return None;
    }
    let left = values[0].floor();
    let top = values[1].floor();
    let right = (values[0] + values[2]).ceil();
    let bottom = (values[1] + values[3]).ceil();
    if !right.is_finite()
        || !bottom.is_finite()
        || left < f64::from(canvas.left)
        || top < f64::from(canvas.top)
        || right > f64::from(canvas.left) + f64::from(canvas.width)
        || bottom > f64::from(canvas.top) + f64::from(canvas.height)
        || right <= left
        || bottom <= top
    {
        return None;
    }
    Some(Rect {
        x: (left - f64::from(canvas.left)) as u32,
        y: (top - f64::from(canvas.top)) as u32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

fn has_region_anchor(canvas: ScreenRect, clip: Rect, values: &[f64]) -> bool {
    !values.is_empty()
        && values.len().is_multiple_of(4)
        && values.chunks_exact(4).any(|chunk| {
            region_coordinate_rect(canvas, chunk)
                .is_some_and(|bounds| safe_intersection(bounds, clip).is_some())
        })
}

fn region_line_bounds(canvas: ScreenRect, clip: Rect, values: &[f64]) -> Option<Rect> {
    if values.is_empty() || !values.len().is_multiple_of(4) || values.len() > MAX_RECTANGLE_VALUES {
        return None;
    }
    let mut bounds: Option<Rect> = None;
    for chunk in values.chunks_exact(4) {
        let current = region_coordinate_rect(canvas, chunk)?;
        if !contains_rect(clip, current) {
            return None;
        }
        bounds = Some(match bounds {
            None => current,
            Some(old) => {
                let x = old.x.min(current.x);
                let y = old.y.min(current.y);
                Rect {
                    x,
                    y,
                    width: (old.x + old.width).max(current.x + current.width) - x,
                    height: (old.y + old.height).max(current.y + current.height) - y,
                }
            }
        });
    }
    bounds
}

struct RectangleArray(*mut SAFEARRAY);
impl RectangleArray {
    fn finish(&mut self) -> Result<()> {
        if !self.0.is_null() {
            unsafe { SafeArrayDestroy(self.0) }?;
            self.0 = std::ptr::null_mut();
        }
        Ok(())
    }
    fn coordinates(&self, budget: &Budget) -> Result<Vec<f64>> {
        if self.0.is_null() {
            return Ok(Vec::new());
        }
        budget.check()?;
        if unsafe { SafeArrayGetDim(self.0) } != 1
            || unsafe { SafeArrayGetElemsize(self.0) } != std::mem::size_of::<f64>() as u32
        {
            return Err("文字行 SAFEARRAY 的维度或元素尺寸无效".into());
        }
        if unsafe { SafeArrayGetVartype(self.0) }? != VT_R8 {
            return Err("文字行 SAFEARRAY 不是 VT_R8".into());
        }
        let lower = unsafe { SafeArrayGetLBound(self.0, 1) }?;
        let upper = unsafe { SafeArrayGetUBound(self.0, 1) }?;
        let Some(count) = region_array_count(lower, upper) else {
            return Err("文字行 SAFEARRAY 长度无效或超限".into());
        };
        let mut values = Vec::new();
        values.try_reserve(count)?;
        for offset in 0..count {
            budget.check()?;
            let index = i32::try_from(i64::from(lower) + offset as i64)?;
            let mut value = 0.0_f64;
            unsafe { SafeArrayGetElement(self.0, &index, (&mut value as *mut f64).cast()) }?;
            values.push(value);
        }
        budget.check()?;
        Ok(values)
    }
}
impl Drop for RectangleArray {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn region_array_count(lower: i32, upper: i32) -> Option<usize> {
    let count = i64::from(upper) - i64::from(lower) + 1;
    (count >= 0 && count <= MAX_RECTANGLE_VALUES as i64 && count % 4 == 0).then_some(count as usize)
}

fn range_coordinates(budget: &Budget, range: &IUIAutomationTextRange) -> Result<Vec<f64>> {
    budget.check()?;
    let mut raw = std::ptr::null_mut();
    let result = unsafe { (range.vtable().GetBoundingRectangles)(range.as_raw(), &mut raw) };
    // 先接管返回的 SAFEARRAY，HRESULT 或预算失败也走同一个 checked destroy。
    let mut array = RectangleArray(raw);
    let answer = (|| -> Result<_> {
        result.ok()?;
        budget.check()?;
        array.coordinates(budget)
    })();
    let cleanup = array.finish();
    cleanup?;
    answer
}

struct ComScope;
impl ComScope {
    fn new() -> Result<Self> {
        // STA / UI 线程已有 apartment 会以 RPC_E_CHANGED_MODE 失败；不能偷偷切换调用者。
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self)
    }
}
impl Drop for ComScope {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct DpiScope(Option<DPI_AWARENESS_CONTEXT>);
impl DpiScope {
    fn new() -> Result<Self> {
        let old =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        if old.0.is_null() {
            return Err("控件线程无法设置物理 DPI".into());
        }
        Ok(Self(Some(old)))
    }
    fn restore(&mut self) -> Result<()> {
        if let Some(old) = self.0 {
            if unsafe { SetThreadDpiAwarenessContext(old) }.0.is_null() {
                return Err("控件线程无法恢复 DPI".into());
            }
            self.0 = None;
        }
        Ok(())
    }
}
impl Drop for DpiScope {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn automation() -> Result<IUIAutomation2> {
    automation_with_timeout(PROVIDER_TIMEOUT_MS)
}

fn automation_with_timeout(timeout_ms: u32) -> Result<IUIAutomation2> {
    let automation: IUIAutomation2 = unsafe {
        CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| uia_error("CoCreateInstance", &error))?
    };
    unsafe {
        automation
            .SetAutoSetFocus(false)
            .map_err(|error| uia_error("SetAutoSetFocus", &error))?;
        automation
            .SetConnectionTimeout(timeout_ms)
            .map_err(|error| uia_error("SetConnectionTimeout", &error))?;
        automation
            .SetTransactionTimeout(timeout_ms)
            .map_err(|error| uia_error("SetTransactionTimeout", &error))?;
    }
    Ok(automation)
}

struct Budget {
    start: Instant,
    duration: Duration,
    nodes: usize,
    node_limit: usize,
    cancellation: Option<ProbeCancellation>,
    spatial_scope: Option<(ScreenRect, Rect)>,
}
impl Budget {
    fn new(duration: Duration) -> Self {
        Self::limited(duration, MAX_NODES)
    }
    fn limited(duration: Duration, node_limit: usize) -> Self {
        Self {
            start: Instant::now(),
            duration,
            nodes: 0,
            node_limit,
            cancellation: None,
            spatial_scope: None,
        }
    }
    fn check(&self) -> Result<()> {
        self.check_current()?;
        if self.start.elapsed() >= self.duration {
            Err("控件探测超过预算，回退窗口选区".into())
        } else {
            Ok(())
        }
    }
    fn check_current(&self) -> Result<()> {
        if let Some(cancellation) = &self.cancellation {
            cancellation.check_current()?;
            if let Some((canvas, rect)) = self.spatial_scope {
                cancellation.check_scope(canvas, rect)?;
            }
        }
        Ok(())
    }
    fn visit(&mut self) -> Result<()> {
        self.check()?;
        if self.nodes >= self.node_limit {
            return Err("控件树超过节点上限".into());
        }
        self.nodes += 1;
        Ok(())
    }
}

fn uia_error(operation: &str, error: &windows::core::Error) -> String {
    format!("UIA {operation} 失败（0x{:08X}）", error.code().0 as u32)
}

#[track_caller]
fn checked<T>(budget: &Budget, call: impl FnOnce() -> windows::core::Result<T>) -> Result<T> {
    budget.check()?;
    let location = std::panic::Location::caller();
    let value = call()
        .map_err(|error| uia_error(&format!("control_selection.rs:{}", location.line()), &error))?;
    budget.check()?;
    Ok(value)
}

fn window_handle(target: &WindowTarget) -> HWND {
    HWND(target.hwnd as *mut c_void)
}

fn validate_window(canvas: ScreenRect, target: &WindowTarget) -> Result<()> {
    if canvas.width > i32::MAX as u32
        || canvas.height > i32::MAX as u32
        || !target.rect.valid(canvas.width, canvas.height)
        || target.hwnd == 0
        || target.process_id == 0
        || target.process_id == std::process::id()
        || target.thread_id == 0
    {
        return Err("控件源窗口身份无效".into());
    }
    let hwnd = window_handle(target);
    let mut process_id = 0;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    let mut bounds = RECT::default();
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool()
            || !IsWindowVisible(hwnd).as_bool()
            || IsIconic(hwnd).as_bool()
            || process_id != target.process_id
            || thread_id != target.thread_id
        {
            return Err("控件源窗口已失效或被替换".into());
        }
        GetWindowRect(hwnd, &mut bounds)?;
    }
    if !same_native_rect(bounds, target.global_bounds)
        || clipped_rect(
            canvas,
            crate::selection_assist::visible_window_bounds(hwnd, bounds),
        ) != Some(target.rect)
    {
        return Err("控件源窗口已移动，不匹配冻结画面".into());
    }
    Ok(())
}

fn validate_root(
    budget: &Budget,
    root: &IUIAutomationElement,
    target: &WindowTarget,
) -> Result<()> {
    let process_id = checked(budget, || unsafe { root.CurrentProcessId() })?;
    if process_id <= 0 || process_id as u32 != target.process_id {
        return Err("UIA 根元素不属于原窗口进程".into());
    }
    Ok(())
}

struct Node {
    element: IUIAutomationElement,
    raw_rect: RECT,
    rect: Rect,
    password: bool,
}

fn path_nodes(
    automation: &IUIAutomation2,
    root: &IUIAutomationElement,
    canvas: ScreenRect,
    target: &WindowTarget,
    point: (i32, i32),
    budget: &mut Budget,
) -> Result<(Vec<Node>, ControlPathState)> {
    let control = checked(budget, || unsafe { automation.ControlViewWalker() })?;
    let nodes = descend(&control, root, canvas, target, point, budget)?;
    if nodes.iter().any(|node| node.rect != target.rect) {
        return Ok((nodes, ControlPathState::ControlView));
    }
    let raw = checked(budget, || unsafe { automation.RawViewWalker() })?;
    let nodes = descend(&raw, root, canvas, target, point, budget)?;
    let state = if nodes.iter().any(|node| node.rect != target.rect) {
        ControlPathState::RawView
    } else {
        ControlPathState::WindowOnly
    };
    Ok((nodes, state))
}

fn descend(
    walker: &IUIAutomationTreeWalker,
    root: &IUIAutomationElement,
    canvas: ScreenRect,
    target: &WindowTarget,
    point: (i32, i32),
    budget: &mut Budget,
) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    let mut parent = root.clone();
    let local = local_point(canvas, point).ok_or("控件物理点越界")?;
    let mut parent_rect = target.rect;
    for _ in 0..MAX_DEPTH {
        let mut child = relative_element(walker, &parent, true, budget)?;
        let mut best: Option<Node> = None;
        while let Some(element) = child {
            budget.visit()?;
            let raw_rect = checked(budget, || unsafe { element.CurrentBoundingRectangle() })?;
            let offscreen = checked(budget, || unsafe { element.CurrentIsOffscreen() })?.as_bool();
            let candidate_rect =
                clipped_rect(canvas, raw_rect).and_then(|r| safe_intersection(r, parent_rect));
            if !offscreen && candidate_rect.is_some_and(|r| r.contains(local)) {
                let rect = candidate_rect.ok_or("控件矩形无效")?;
                let process_id = checked(budget, || unsafe { element.CurrentProcessId() })?;
                if process_id as u32 == std::process::id() {
                    return Err("UIA 子树命中自身覆盖层".into());
                }
                let password =
                    checked(budget, || unsafe { element.CurrentIsPassword() })?.as_bool();
                // 选区只需要几何与身份。Name 是提示标签，额外的远程名称请求可能
                // 超时或返回大段内容；保持为空，由界面使用“UIA 控件”提示即可。
                if best.as_ref().is_none_or(|old| area(rect) < area(old.rect)) {
                    best = Some(Node {
                        element: element.clone(),
                        raw_rect,
                        rect,
                        password,
                    });
                }
            }
            child = relative_element(walker, &element, false, budget)?;
        }
        let Some(best) = best else { return Ok(nodes) };
        parent = best.element.clone();
        parent_rect = best.rect;
        nodes.push(best);
    }
    // 到达上限仍有子层就不把半截路径当作完整控件定位。
    if relative_element(walker, &parent, true, budget)?.is_some() {
        return Err("控件树超过深度上限".into());
    }
    Ok(nodes)
}

fn relative_element(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
    first_child: bool,
    budget: &Budget,
) -> Result<Option<IUIAutomationElement>> {
    budget.check()?;
    let mut raw = std::ptr::null_mut();
    // 保留合法 S_OK + NULL 的树尾语义；生成的非可空接口包装会将 NULL 转为错误。
    let result: HRESULT = unsafe {
        let vtable = walker.vtable();
        if first_child {
            (vtable.GetFirstChildElement)(walker.as_raw(), element.as_raw(), &mut raw)
        } else {
            (vtable.GetNextSiblingElement)(walker.as_raw(), element.as_raw(), &mut raw)
        }
    };
    // 接管返回引用后再检查预算，确保超时路径也释放已经返回的 COM 对象。
    let returned = (!raw.is_null()).then(|| unsafe { IUIAutomationElement::from_raw(raw) });
    result.ok().map_err(|error| {
        uia_error(
            if first_child {
                "GetFirstChildElement"
            } else {
                "GetNextSiblingElement"
            },
            &error,
        )
    })?;
    budget.check()?;
    Ok(returned)
}

fn hierarchy_targets(target: &WindowTarget, descendants: Vec<ControlTarget>) -> Vec<ControlTarget> {
    let mut targets: Vec<ControlTarget> = Vec::new();
    for node in descendants.into_iter().rev() {
        if node.rect != target.rect && !targets.iter().any(|existing| existing.rect == node.rect) {
            targets.push(node);
        }
    }
    targets.push(ControlTarget {
        rect: target.rect,
        title: target.title.clone(),
    });
    targets
}

fn element_text(budget: &Budget, element: &IUIAutomationElement) -> Result<Option<String>> {
    element_text_with_sources(
        || {
            let Some(pattern) =
                current_pattern::<IUIAutomationTextPattern>(budget, element, UIA_TextPatternId)?
            else {
                return Ok(None);
            };
            let range = checked(budget, || unsafe { pattern.DocumentRange() })?;
            let value = checked(budget, || unsafe {
                range.GetText((MAX_TEXT_UNITS + 1) as i32)
            })?;
            Ok(Some(value.to_string()))
        },
        || {
            // 同一个元素：合法空 TextPattern 也可退到 ValuePattern，不读取 CurrentName。
            let Some(pattern) =
                current_pattern::<IUIAutomationValuePattern>(budget, element, UIA_ValuePatternId)?
            else {
                return Ok(None);
            };
            let value = checked(budget, || unsafe { pattern.CurrentValue() })?;
            Ok(Some(value.to_string()))
        },
    )
}

/// 只在 TextPattern 合法无正文时调用后备来源；错误与超长正文均阻止回退。
fn element_text_with_sources(
    text: impl FnOnce() -> Result<Option<String>>,
    value: impl FnOnce() -> Result<Option<String>>,
) -> Result<Option<String>> {
    if let Some(raw) = text()? {
        if let Some(text) = bounded_text(raw)? {
            return Ok(Some(text));
        }
    }
    match value()? {
        Some(raw) => bounded_text(raw),
        None => Ok(None),
    }
}

fn current_pattern<T: Interface>(
    budget: &Budget,
    element: &IUIAutomationElement,
    pattern: UIA_PATTERN_ID,
) -> Result<Option<T>> {
    budget.check()?;
    let mut raw = std::ptr::null_mut();
    let result =
        unsafe { (element.vtable().GetCurrentPattern)(element.as_raw(), pattern, &mut raw) };
    let returned = (!raw.is_null()).then(|| unsafe { IUnknown::from_raw(raw) });
    budget.check()?;
    if result.0 as u32 == UIA_E_NOTSUPPORTED {
        return Ok(None);
    }
    // 除“不支持模式”以外的 provider 错误（含超时）不得当作已完成探测。
    result.ok()?;
    returned
        .map(|value| value.cast::<T>().map_err(Into::into))
        .transpose()
}

fn bounded_text(value: String) -> Result<Option<String>> {
    if value.encode_utf16().count() > MAX_TEXT_UNITS {
        return Err("控件正文超过长度上限，使用选区 OCR".into());
    }
    Ok((!value.trim().is_empty()).then_some(value))
}

fn text_covers_selection(selection: Rect, element: Rect) -> bool {
    element.width > 0 && element.height > 0 && selection == element
}

fn same_native_rect(a: RECT, b: RECT) -> bool {
    a.left == b.left && a.top == b.top && a.right == b.right && a.bottom == b.bottom
}
fn area(rect: Rect) -> u64 {
    u64::from(rect.width) * u64::from(rect.height)
}
fn contains_rect(outer: Rect, inner: Rect) -> bool {
    inner.width > 0
        && inner.height > 0
        && inner.x >= outer.x
        && inner.y >= outer.y
        && u64::from(inner.x) + u64::from(inner.width)
            <= u64::from(outer.x) + u64::from(outer.width)
        && u64::from(inner.y) + u64::from(inner.height)
            <= u64::from(outer.y) + u64::from(outer.height)
}
fn text_window_target(targets: &[WindowTarget], rect: Rect) -> Option<&WindowTarget> {
    let index = targets
        .iter()
        .position(|target| contains_rect(target.rect, rect))?;
    if targets[..index]
        .iter()
        .any(|above| safe_intersection(above.rect, rect).is_some())
    {
        return None;
    }
    Some(&targets[index])
}
fn safe_intersection(a: Rect, b: Rect) -> Option<Rect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (u64::from(a.x) + u64::from(a.width)).min(u64::from(b.x) + u64::from(b.width));
    let bottom = (u64::from(a.y) + u64::from(a.height)).min(u64::from(b.y) + u64::from(b.height));
    (right > u64::from(x) && bottom > u64::from(y)).then(|| Rect {
        x,
        y,
        width: (right - u64::from(x)) as u32,
        height: (bottom - u64::from(y)) as u32,
    })
}
fn clipped_rect(canvas: ScreenRect, bounds: RECT) -> Option<Rect> {
    let left = i64::from(bounds.left).max(i64::from(canvas.left));
    let top = i64::from(bounds.top).max(i64::from(canvas.top));
    let right = i64::from(bounds.right).min(i64::from(canvas.left) + i64::from(canvas.width));
    let bottom = i64::from(bounds.bottom).min(i64::from(canvas.top) + i64::from(canvas.height));
    (right > left && bottom > top).then(|| Rect {
        x: (left - i64::from(canvas.left)) as u32,
        y: (top - i64::from(canvas.top)) as u32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}
fn uncut_local_rect(canvas: ScreenRect, bounds: RECT) -> Option<Rect> {
    if i64::from(bounds.left) < i64::from(canvas.left)
        || i64::from(bounds.top) < i64::from(canvas.top)
        || i64::from(bounds.right) > i64::from(canvas.left) + i64::from(canvas.width)
        || i64::from(bounds.bottom) > i64::from(canvas.top) + i64::from(canvas.height)
    {
        return None;
    }
    clipped_rect(canvas, bounds)
}
fn local_point(canvas: ScreenRect, point: (i32, i32)) -> Option<(i32, i32)> {
    let x = i64::from(point.0) - i64::from(canvas.left);
    let y = i64::from(point.1) - i64::from(canvas.top);
    (x >= 0 && y >= 0 && x < i64::from(canvas.width) && y < i64::from(canvas.height))
        .then(|| Some((i32::try_from(x).ok()?, i32::try_from(y).ok()?)))
        .flatten()
}
fn global_center(canvas: ScreenRect, rect: Rect) -> Option<(i32, i32)> {
    let x = i64::from(canvas.left) + i64::from(rect.x) + i64::from(rect.width / 2);
    let y = i64::from(canvas.top) + i64::from(rect.y) + i64::from(rect.height / 2);
    Some((i32::try_from(x).ok()?, i32::try_from(y).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_desktop_clip_and_point_remain_physical() {
        let canvas = ScreenRect {
            left: -1440,
            top: -200,
            width: 4000,
            height: 1640,
        };
        assert_eq!(
            clipped_rect(
                canvas,
                RECT {
                    left: -1500,
                    top: -250,
                    right: -1000,
                    bottom: 100
                }
            ),
            Some(Rect {
                x: 0,
                y: 0,
                width: 440,
                height: 300
            })
        );
        assert_eq!(local_point(canvas, (-1400, -150)), Some((40, 50)));
        assert_eq!(local_point(canvas, (-1441, -150)), None);
        assert_eq!(
            global_center(
                canvas,
                Rect {
                    x: 40,
                    y: 50,
                    width: 20,
                    height: 30
                }
            ),
            Some((-1390, -135))
        );
    }

    #[test]
    fn text_requires_whole_unclipped_element_inside_roi() {
        let canvas = ScreenRect {
            left: -100,
            top: 0,
            width: 200,
            height: 100,
        };
        let partial = RECT {
            left: -120,
            top: 5,
            right: -70,
            bottom: 30,
        };
        assert!(clipped_rect(canvas, partial).is_some());
        assert!(uncut_local_rect(canvas, partial).is_none());
        let outer = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(contains_rect(
            outer,
            Rect {
                x: 5,
                y: 5,
                width: 20,
                height: 20
            }
        ));
        assert!(!contains_rect(
            outer,
            Rect {
                x: 80,
                y: 5,
                width: 30,
                height: 20
            }
        ));
        assert!(!contains_rect(
            outer,
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 20
            }
        ));
        assert!(!contains_rect(
            outer,
            Rect {
                x: u32::MAX,
                y: 0,
                width: 10,
                height: 20
            }
        ));
        let element = Rect {
            x: 5,
            y: 5,
            width: 20,
            height: 20,
        };
        assert!(text_covers_selection(element, element));
        assert!(!text_covers_selection(outer, element));
        assert!(!text_covers_selection(element, outer));
        assert!(!text_covers_selection(Rect::full(0, 0), Rect::full(0, 0)));
    }

    #[test]
    fn overflow_safe_intersection_and_utf16_text_limit() {
        assert_eq!(
            safe_intersection(
                Rect {
                    x: u32::MAX - 2,
                    y: 0,
                    width: 2,
                    height: 1
                },
                Rect {
                    x: u32::MAX - 1,
                    y: 0,
                    width: 1,
                    height: 1
                }
            ),
            Some(Rect {
                x: u32::MAX - 1,
                y: 0,
                width: 1,
                height: 1
            })
        );
        assert_eq!(bounded_text(" \r\n".into()).unwrap(), None);
        assert_eq!(
            bounded_text("控件正文😀".into()).unwrap(),
            Some("控件正文😀".into())
        );
        assert!(bounded_text("😀".repeat(MAX_TEXT_UNITS / 2 + 1)).is_err());
    }

    #[test]
    fn window_path_clips_native_and_provider_bounds_to_visible_frame() {
        let canvas = ScreenRect {
            left: -1920,
            top: -200,
            width: 3840,
            height: 1280,
        };
        let window = WindowTarget {
            rect: Rect {
                x: 920,
                y: 250,
                width: 800,
                height: 600,
            },
            global_bounds: RECT {
                left: -1007,
                top: 50,
                right: -193,
                bottom: 657,
            },
            title: String::new(),
            hwnd: 1,
            process_id: 2,
            thread_id: 3,
        };
        let root = point_geometry(canvas, window.rect, window.global_bounds, (930, 260)).unwrap();
        assert_eq!(root, window.rect);
        let path = hierarchy_targets(
            &window,
            vec![ControlTarget {
                rect: root,
                title: String::new(),
            }],
        );
        assert_eq!(path.len(), 1);
        assert_eq!(path[0].rect, window.rect);
        for point in [(915, 260), (1725, 260), (930, 855)] {
            assert!(point_geometry(canvas, window.rect, window.global_bounds, point).is_none());
        }
    }

    #[test]
    fn child_to_ancestor_order_deduplicates_rectangles_and_keeps_window_last() {
        let window = WindowTarget {
            rect: Rect::full(800, 600),
            title: "原窗口".into(),
            hwnd: 1,
            global_bounds: RECT {
                left: 0,
                top: 0,
                right: 800,
                bottom: 600,
            },
            process_id: 2,
            thread_id: 3,
        };
        let parent = Rect {
            x: 100,
            y: 50,
            width: 500,
            height: 400,
        };
        let child = Rect {
            x: 120,
            y: 70,
            width: 100,
            height: 40,
        };
        let targets = hierarchy_targets(
            &window,
            vec![
                ControlTarget {
                    rect: window.rect,
                    title: "重复根".into(),
                },
                ControlTarget {
                    rect: parent,
                    title: "父".into(),
                },
                ControlTarget {
                    rect: child,
                    title: "子".into(),
                },
                ControlTarget {
                    rect: child,
                    title: "更深同矩形".into(),
                },
            ],
        );
        assert_eq!(
            targets.iter().map(|target| target.rect).collect::<Vec<_>>(),
            vec![child, parent, window.rect]
        );
        assert_eq!(targets[0].title, "更深同矩形");
        assert_eq!(targets[2].title, "原窗口");
        assert_eq!(hierarchy_targets(&window, Vec::new()).len(), 1);
    }

    #[test]
    fn raw_window_movement_is_visible_when_canvas_clip_is_unchanged() {
        let canvas = ScreenRect {
            left: -100,
            top: -50,
            width: 200,
            height: 100,
        };
        let original = RECT {
            left: -200,
            top: -100,
            right: 200,
            bottom: 100,
        };
        let moved = RECT {
            left: -220,
            top: -100,
            right: 180,
            bottom: 100,
        };
        let resized = RECT {
            left: -200,
            top: -100,
            right: 220,
            bottom: 100,
        };
        assert_eq!(clipped_rect(canvas, original), Some(Rect::full(200, 100)));
        assert_eq!(clipped_rect(canvas, moved), clipped_rect(canvas, original));
        assert_eq!(
            clipped_rect(canvas, resized),
            clipped_rect(canvas, original)
        );
        assert!(!same_native_rect(original, moved));
        assert!(!same_native_rect(original, resized));
        assert!(same_native_rect(original, original));
    }

    #[test]
    fn text_refuses_partial_upper_window_occlusion_but_not_touching_edges() {
        let roi = Rect {
            x: 100,
            y: 100,
            width: 80,
            height: 30,
        };
        let make_window = |hwnd, rect: Rect| WindowTarget {
            rect,
            global_bounds: RECT {
                left: rect.x as i32,
                top: rect.y as i32,
                right: (rect.x + rect.width) as i32,
                bottom: (rect.y + rect.height) as i32,
            },
            title: String::new(),
            hwnd,
            process_id: 2,
            thread_id: 3,
        };
        let lower = make_window(2, Rect::full(800, 600));
        let partially_covering = make_window(
            1,
            Rect {
                x: 140,
                y: 90,
                width: 60,
                height: 30,
            },
        );
        assert!(text_window_target(&[partially_covering, lower.clone()], roi).is_none());
        let touching = make_window(
            1,
            Rect {
                x: 180,
                y: 100,
                width: 60,
                height: 30,
            },
        );
        assert_eq!(
            text_window_target(&[touching, lower.clone()], roi).map(|target| target.hwnd),
            Some(2)
        );
        let covering = make_window(1, roi);
        assert_eq!(
            text_window_target(&[covering, lower.clone()], roi).map(|target| target.hwnd),
            Some(1)
        );
        assert_eq!(
            text_window_target(&[lower], roi).map(|target| target.hwnd),
            Some(2)
        );
    }
}

#[cfg(test)]
mod geometry_query_tests {
    use super::*;

    struct SyntheticNode {
        info: GeometryInfo,
        children: Vec<usize>,
        failure: u8,
    }
    #[derive(Default)]
    struct SyntheticSource {
        nodes: Vec<SyntheticNode>,
        queried: Vec<usize>,
        cancel_after_query: Option<(usize, Arc<AtomicU64>)>,
        move_after_query: Vec<(usize, (i32, i32))>,
        latest_point: Option<Arc<AtomicU64>>,
    }
    impl SyntheticSource {
        fn add(&mut self, bounds: RECT, kind: i32, children: Vec<usize>) -> usize {
            let index = self.nodes.len();
            self.nodes.push(SyntheticNode {
                info: GeometryInfo {
                    bounds,
                    kind,
                    selected_tab: false,
                },
                children,
                failure: 0,
            });
            index
        }
    }
    impl GeometrySource for SyntheticSource {
        type Node = usize;
        fn info(&self, node: &usize) -> GeometryInfo {
            self.nodes[*node].info
        }
        fn children(
            &mut self,
            node: &usize,
            _: usize,
            _: &mut Budget,
        ) -> Result<(Vec<usize>, bool)> {
            self.queried.push(*node);
            if let Some((after, generation)) = &self.cancel_after_query {
                if after == node {
                    generation.fetch_add(1, Ordering::AcqRel);
                }
            }
            if let Some(latest_point) = &self.latest_point {
                for (after, point) in &self.move_after_query {
                    if after == node {
                        latest_point.store(pack_probe_point(*point), Ordering::Release);
                    }
                }
            }
            match self.nodes[*node].failure {
                1 => Err("合成慢 provider".into()),
                2 => Err(Box::new(GeometryInvalidated)),
                _ => Ok((self.nodes[*node].children.clone(), true)),
            }
        }
    }
    fn bounds(x: i32, y: i32, width: i32, height: i32) -> RECT {
        RECT {
            left: x,
            top: y,
            right: x + width,
            bottom: y + height,
        }
    }
    fn search(
        source: &mut SyntheticSource,
        root: usize,
        point: (i32, i32),
        limit: usize,
    ) -> GeometryBranch {
        search_with_budget(
            source,
            root,
            point,
            &mut Budget::limited(Duration::from_secs(10), limit),
        )
    }
    fn search_with_budget(
        source: &mut SyntheticSource,
        root: usize,
        point: (i32, i32),
        budget: &mut Budget,
    ) -> GeometryBranch {
        geometry_search(
            &root,
            source,
            ScreenRect {
                left: 0,
                top: 0,
                width: 800,
                height: 600,
            },
            Rect::full(800, 600),
            point,
            0,
            budget,
        )
    }
    #[test]
    fn geometry_tree_resets_before_cache_limit_instead_of_failing() {
        let mut tree = GeometryTree::default();
        assert!(!tree.near_full());
        tree.nodes = MAX_GEOMETRY_NODES / 2 - 1;
        assert!(!tree.near_full());
        tree.nodes = MAX_GEOMETRY_NODES / 2;
        assert!(tree.near_full());
        assert!(GEOMETRY_CACHE_LEASE >= Duration::from_secs(5));
    }
    #[test]
    fn completed_point_path_beats_smaller_failed_control_or_container() {
        for kind in [50000, 50030] {
            let mut source = SyntheticSource::default();
            let failed = source.add(bounds(105, 105, 12, 12), kind, vec![]);
            source.nodes[failed].failure = 1;
            let leaf = source.add(bounds(100, 100, 80, 30), 50000, vec![]);
            let root = source.add(bounds(0, 0, 800, 600), 50032, vec![failed, leaf]);
            let result = search(&mut source, root, (110, 110), MAX_GEOMETRY_NODES);
            assert_eq!(
                result.targets.last().unwrap().rect,
                Rect {
                    x: 100,
                    y: 100,
                    width: 80,
                    height: 30
                }
            );
            assert!(result.complete && result.terminal);
            assert_eq!(
                source.queried,
                vec![root, failed, leaf],
                "失败小分支之后已实际查到了完整叶"
            );
        }
    }

    #[test]
    fn later_smaller_incomplete_wrapper_path_keeps_already_completed_point_leaf() {
        let mut source = SyntheticSource::default();
        let leaf = source.add(bounds(100, 100, 80, 30), 50000, vec![]);
        let failed = source.add(bounds(105, 105, 12, 12), 50000, vec![]);
        source.nodes[failed].failure = 1;
        let wrapper = source.add(bounds(90, 90, 200, 100), 50033, vec![failed]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![leaf, wrapper]);
        let result = search(&mut source, root, (110, 110), MAX_GEOMETRY_NODES);
        assert_eq!(
            result.targets.last().unwrap().rect,
            Rect {
                x: 100,
                y: 100,
                width: 80,
                height: 30
            }
        );
        assert!(result.complete && result.terminal);
        assert_eq!(
            source.queried,
            vec![root, leaf, wrapper, failed],
            "不能丢掉先前已核完整的元素"
        );
    }

    #[test]
    fn completed_blank_container_and_same_area_leaf_keep_existing_geometry_order() {
        let mut source = SyntheticSource::default();
        let blank = source.add(bounds(105, 105, 12, 12), 50030, vec![]);
        let leaf = source.add(bounds(100, 100, 80, 30), 50000, vec![]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![blank, leaf]);
        let result = search(&mut source, root, (110, 110), MAX_GEOMETRY_NODES);
        assert_eq!(
            result.targets.last().unwrap().rect,
            Rect {
                x: 105,
                y: 105,
                width: 12,
                height: 12
            }
        );
        assert!(
            result.complete && !result.terminal,
            "完整容器空白仍是当前元素，不伪装终端类型"
        );

        let mut source = SyntheticSource::default();
        let shallow = source.add(bounds(100, 100, 80, 30), 50000, vec![]);
        let deep = source.add(bounds(100, 100, 80, 30), 50000, vec![]);
        let owner = source.add(bounds(100, 100, 80, 30), 50033, vec![deep]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![shallow, owner]);
        let result = search(&mut source, root, (110, 110), MAX_GEOMETRY_NODES);
        assert!(result.complete && result.terminal);
        assert_eq!(
            result.targets.len(),
            3,
            "同面积完整叶沿原深度规则保留所有权路径"
        );
        assert_eq!(source.queried, vec![root, shallow, owner, deep]);
    }

    #[test]
    fn equal_area_ownership_peer_resolves_later_document_leaf() {
        let mut source = SyntheticSource::default();
        let empty_owner = source.add(bounds(0, 0, 800, 600), 50025, vec![]);
        let leaf = source.add(bounds(100, 100, 100, 40), 50000, vec![]);
        let document = source.add(bounds(0, 0, 800, 600), 50030, vec![leaf]);
        let renderer = source.add(bounds(0, 0, 800, 600), 50033, vec![document]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![empty_owner, renderer]);
        let result = search(&mut source, root, (120, 120), MAX_GEOMETRY_NODES);
        assert_eq!(
            result.targets.last().unwrap().rect,
            Rect {
                x: 100,
                y: 100,
                width: 100,
                height: 40
            }
        );
        assert!(result.terminal && result.complete);
        assert!(source.queried.contains(&renderer));
    }

    #[test]
    fn cancelled_geometry_query_stops_before_leaf_and_does_not_claim_completion() {
        let mut source = SyntheticSource::default();
        let leaf = source.add(bounds(100, 100, 100, 40), 50000, vec![]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![leaf]);
        let generation = Arc::new(AtomicU64::new(7));
        source.cancel_after_query = Some((root, generation.clone()));
        let mut budget = Budget::limited(Duration::from_secs(10), MAX_GEOMETRY_NODES);
        budget.cancellation = Some(ProbeCancellation::new(
            generation,
            7,
            Arc::new(AtomicU64::new(pack_probe_point((120, 120)))),
        ));
        let result = geometry_search(
            &root,
            &mut source,
            ScreenRect {
                left: 0,
                top: 0,
                width: 800,
                height: 600,
            },
            Rect::full(800, 600),
            (120, 120),
            0,
            &mut budget,
        );
        assert_eq!(source.queried, vec![root]);
        assert!(!result.terminal && !result.complete);
        assert!(budget.check().is_err());
    }

    #[test]
    fn cancelled_probe_checks_before_and_after_provider_call() {
        let generation = Arc::new(AtomicU64::new(7));
        let mut budget = Budget::limited(Duration::from_secs(10), MAX_GEOMETRY_NODES);
        budget.cancellation = Some(ProbeCancellation::new(
            generation.clone(),
            7,
            Arc::new(AtomicU64::new(pack_probe_point((120, 120)))),
        ));
        let after_call = checked(&budget, || -> windows::core::Result<u32> {
            generation.fetch_add(1, Ordering::AcqRel);
            Ok(123)
        });
        assert!(after_call.is_err());
        let mut called_again = false;
        let before_call = checked(&budget, || -> windows::core::Result<u32> {
            called_again = true;
            Ok(456)
        });
        assert!(before_call.is_err());
        assert!(!called_again);
    }

    #[test]
    fn continuous_motion_within_leaf_does_not_starve_complete_geometry() {
        let mut source = SyntheticSource::default();
        let leaf = source.add(bounds(100, 100, 100, 40), 50000, vec![]);
        let wrapper = source.add(bounds(0, 0, 800, 600), 50030, vec![leaf]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![wrapper]);
        let latest_point = Arc::new(AtomicU64::new(pack_probe_point((120, 120))));
        source.latest_point = Some(latest_point.clone());
        source.move_after_query = vec![
            (root, (130, 120)),
            (wrapper, (140, 121)),
            (leaf, (150, 122)),
        ];
        let mut budget = Budget::limited(Duration::from_secs(10), MAX_GEOMETRY_NODES);
        budget.cancellation = Some(ProbeCancellation::new(
            Arc::new(AtomicU64::new(7)),
            7,
            latest_point,
        ));
        let result = search_with_budget(&mut source, root, (120, 120), &mut budget);
        assert!(result.complete && result.terminal);
        assert!(result.targets.last().unwrap().rect.contains((150, 122)));
        assert!(budget.check().is_ok());
    }

    #[test]
    fn leaving_entered_leaf_cancels_rest_of_query_and_stays_cancelled() {
        let mut source = SyntheticSource::default();
        let leaf = source.add(bounds(100, 100, 100, 40), 50000, vec![]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![leaf]);
        let latest_point = Arc::new(AtomicU64::new(pack_probe_point((120, 120))));
        source.latest_point = Some(latest_point.clone());
        source.move_after_query = vec![(leaf, (300, 300))];
        let mut budget = Budget::limited(Duration::from_secs(10), MAX_GEOMETRY_NODES);
        let cancellation =
            ProbeCancellation::new(Arc::new(AtomicU64::new(7)), 7, latest_point.clone());
        budget.cancellation = Some(cancellation.clone());
        let result = search_with_budget(&mut source, root, (120, 120), &mut budget);
        assert!(!result.complete && !result.terminal);
        assert!(cancellation.check_current().is_err());
        latest_point.store(pack_probe_point((120, 120)), Ordering::Release);
        assert!(cancellation.check_current().is_err());
    }

    #[test]
    fn sibling_search_restores_scope_before_motion_in_new_sibling() {
        let mut source = SyntheticSource::default();
        let failed = source.add(bounds(100, 100, 80, 30), 50000, vec![]);
        source.nodes[failed].failure = 1;
        let peer = source.add(bounds(150, 100, 80, 30), 50000, vec![]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![failed, peer]);
        let latest_point = Arc::new(AtomicU64::new(pack_probe_point((160, 110))));
        source.latest_point = Some(latest_point.clone());
        source.move_after_query = vec![(peer, (200, 110))];
        let mut budget = Budget::limited(Duration::from_secs(10), MAX_GEOMETRY_NODES);
        budget.cancellation = Some(ProbeCancellation::new(
            Arc::new(AtomicU64::new(7)),
            7,
            latest_point,
        ));
        let result = search_with_budget(&mut source, root, (160, 110), &mut budget);
        assert!(result.complete && result.terminal);
        assert_eq!(result.targets.last().unwrap().rect.x, 150);
        assert!(budget.check().is_ok());
        assert!(budget.spatial_scope.is_none());
    }

    #[test]
    fn ownership_wrapper_bounds_do_not_cancel_motion_inside_owned_leaf() {
        let mut source = SyntheticSource::default();
        let leaf = source.add(bounds(100, 200, 200, 30), 50004, vec![]);
        let tab = source.add(bounds(0, 0, 80, 20), 50019, vec![leaf]);
        source.nodes[tab].info.selected_tab = true;
        let wrapper = source.add(RECT::default(), 50033, vec![tab]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![wrapper]);
        let latest_point = Arc::new(AtomicU64::new(pack_probe_point((150, 210))));
        source.latest_point = Some(latest_point.clone());
        source.move_after_query = vec![(tab, (175, 215))];
        let mut budget = Budget::limited(Duration::from_secs(10), MAX_GEOMETRY_NODES);
        budget.cancellation = Some(ProbeCancellation::new(
            Arc::new(AtomicU64::new(7)),
            7,
            latest_point,
        ));
        let result = search_with_budget(&mut source, root, (150, 210), &mut budget);
        assert!(result.complete && result.terminal);
        assert!(result.targets.last().unwrap().rect.contains((175, 215)));
        assert!(budget.check().is_ok());
    }
    #[test]
    fn empty_and_wrong_dpi_wrappers_are_skipped_but_final_leaf_contains_point() {
        let mut source = SyntheticSource::default();
        let leaf = source.add(bounds(500, 300, 80, 25), 50020, vec![]);
        let wrong_dpi = source.add(bounds(0, 0, 400, 200), 50033, vec![leaf]);
        let empty = source.add(RECT::default(), 50026, vec![wrong_dpi]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![empty]);
        let result = search(&mut source, root, (520, 310), MAX_GEOMETRY_NODES);
        assert!(result.terminal && result.complete);
        assert_eq!(result.targets.len(), 2);
        assert_eq!(result.targets.last().unwrap().rect.x, 500);
        let outside = search(&mut source, root, (650, 400), MAX_GEOMETRY_NODES);
        assert!(!outside.terminal);
        assert_eq!(outside.targets.len(), 1);
    }
    #[test]
    fn selected_tab_can_own_content_below_header_inactive_tab_is_not_walked() {
        let mut source = SyntheticSource::default();
        let leaf = source.add(bounds(100, 200, 200, 30), 50004, vec![]);
        let tab = source.add(bounds(0, 0, 80, 20), 50019, vec![leaf]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![tab]);
        let inactive = search(&mut source, root, (150, 210), MAX_GEOMETRY_NODES);
        assert!(!inactive.terminal && !source.queried.contains(&tab));
        source.nodes[tab].info.selected_tab = true;
        let selected = search(&mut source, root, (150, 210), MAX_GEOMETRY_NODES);
        assert!(selected.terminal && selected.complete);
        assert_eq!(selected.targets.last().unwrap().rect.width, 200);
    }
    #[test]
    fn large_sibling_batch_does_not_spend_one_remote_call_per_sibling() {
        let mut source = SyntheticSource::default();
        let mut children = Vec::new();
        for _ in 0..300 {
            children.push(source.add(bounds(0, 0, 10, 10), 50000, vec![]));
        }
        let leaf = source.add(bounds(300, 300, 100, 30), 50000, vec![]);
        children.push(leaf);
        let root = source.add(bounds(0, 0, 800, 600), 50032, children);
        let result = search(&mut source, root, (350, 310), 16);
        assert!(result.terminal && result.complete);
        assert_eq!(source.queried, vec![root, leaf]);
    }
    #[test]
    fn provider_failure_and_node_limit_keep_safe_geometry_without_claiming_leaf() {
        let mut source = SyntheticSource::default();
        let leaf = source.add(bounds(100, 100, 100, 40), 50000, vec![]);
        source.nodes[leaf].failure = 1;
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![leaf]);
        let failed = search(&mut source, root, (120, 120), MAX_GEOMETRY_NODES);
        assert_eq!(failed.targets.last().unwrap().rect.width, 100);
        assert!(!failed.complete && !failed.terminal);
        source.nodes[leaf].failure = 0;
        let bounded = search(&mut source, root, (120, 120), 1);
        assert_eq!(bounded.targets.last().unwrap().rect.width, 100);
        assert!(!bounded.complete && !bounded.terminal);
    }
    #[test]
    fn observed_moved_child_discards_its_geometry_and_does_not_discard_safe_peer() {
        let mut source = SyntheticSource::default();
        let moved = source.add(bounds(100, 100, 20, 20), 50000, vec![]);
        source.nodes[moved].failure = 2;
        let safe = source.add(bounds(95, 95, 50, 30), 50000, vec![]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![moved, safe]);
        let result = search(&mut source, root, (110, 110), MAX_GEOMETRY_NODES);
        assert_eq!(result.targets.last().unwrap().rect.width, 50);
        assert!(result.complete && result.terminal);
    }
    #[test]
    fn structural_leaf_and_clipped_away_child_do_not_claim_terminal_accuracy() {
        let mut source = SyntheticSource::default();
        let structure = source.add(bounds(100, 100, 200, 100), 50030, vec![]);
        let outside = source.add(bounds(850, 100, 30, 30), 50000, vec![]);
        let root = source.add(bounds(0, 0, 800, 600), 50032, vec![structure, outside]);
        let result = search(&mut source, root, (150, 130), MAX_GEOMETRY_NODES);
        assert!(result.complete && !result.terminal);
        assert_eq!(result.targets.last().unwrap().rect.width, 200);
        assert!(point_geometry(
            ScreenRect {
                left: -1440,
                top: -200,
                width: 4000,
                height: 1640
            },
            Rect::full(4000, 1640),
            bounds(-1400, -150, 100, 25),
            (50, 60)
        )
        .is_some());
    }

    #[test]
    fn native_class_fallback_only_claims_classic_leaf_precision() {
        for class in ["Button", "BUTTON", "Edit", "Static", "ScrollBar"] {
            assert!(native_terminal_class(class, 0));
        }
        for class in [
            "",
            "Chrome_RenderWidgetHostHWND",
            "DirectUIHWND",
            "SysListView32",
            "RichEditD2DPT",
        ] {
            assert!(!native_terminal_class(class, 0));
        }
        assert!(!native_terminal_class("Button", BS_GROUPBOX));
        assert!(!native_terminal_class("Button", BS_GROUPBOX | 0x40000000));
    }
}

#[cfg(test)]
mod region_text_tests {
    use super::*;

    #[test]
    fn observed_ancestor_change_wins_over_post_call_timeout() {
        let expired = Budget::new(Duration::ZERO);
        let changed = region_ancestor_observation(&expired, true).unwrap_err();
        assert!(changed.is::<RegionAncestorChanged>());
        let unknown = region_ancestor_observation(&expired, false).unwrap_err();
        assert!(!unknown.is::<RegionAncestorChanged>());
        let fresh = Budget::new(Duration::from_secs(60));
        assert!(region_ancestor_observation(&fresh, false).is_ok());
        assert!(region_ancestor_observation(&fresh, true)
            .unwrap_err()
            .is::<RegionAncestorChanged>());
    }

    fn canvas() -> ScreenRect {
        ScreenRect {
            left: -1440,
            top: -200,
            width: 4000,
            height: 1640,
        }
    }
    fn line_rect() -> Rect {
        Rect {
            x: 40,
            y: 50,
            width: 120,
            height: 20,
        }
    }

    #[test]
    fn complete_line_geometry_preserves_negative_origin_and_rejects_partial_fragments() {
        let clip = Rect {
            x: 0,
            y: 0,
            width: 400,
            height: 300,
        };
        let fragments = [-1400.0, -150.0, 50.0, 20.0, -1340.0, -150.0, 60.0, 20.0];
        assert_eq!(
            region_line_bounds(canvas(), clip, &fragments),
            Some(line_rect())
        );
        let too_narrow = Rect { width: 159, ..clip };
        assert_eq!(region_line_bounds(canvas(), too_narrow, &fragments), None);
        assert!(has_region_anchor(canvas(), too_narrow, &fragments));
        assert_eq!(
            region_line_bounds(canvas(), clip, &[-1441.0, -150.0, 50.0, 20.0]),
            None
        );
        assert_eq!(
            region_line_bounds(canvas(), clip, &[-1400.1, -150.1, 120.0, 20.0]),
            Some(Rect {
                x: 39,
                y: 49,
                width: 121,
                height: 21
            })
        );
    }

    #[test]
    fn invalid_array_shape_nonfinite_sentinel_and_overflow_never_form_a_line() {
        let clip = Rect::full(4000, 1640);
        for values in [
            vec![],
            vec![0.0; 3],
            vec![-1400.0, -150.0, 1.0, 1.0],
            vec![-1400.0, f64::NAN, 30.0, 20.0],
            vec![-1400.0, -150.0, f64::INFINITY, 20.0],
            vec![f64::MAX, -150.0, f64::MAX, 20.0],
        ] {
            assert!(region_line_bounds(canvas(), clip, &values).is_none());
        }
        assert_eq!(region_array_count(7, 14), Some(8));
        assert_eq!(region_array_count(0, -1), Some(0));
        assert_eq!(region_array_count(0, 4), None);
        assert_eq!(region_array_count(i32::MIN, i32::MAX), None);
        assert_eq!(region_array_count(0, MAX_RECTANGLE_VALUES as i32), None);
    }

    #[test]
    fn whole_value_bounds_are_not_repaired_by_clipping() {
        let partial = RECT {
            left: -1450,
            top: -150,
            right: -1350,
            bottom: -120,
        };
        assert!(clipped_rect(canvas(), partial).is_some());
        assert!(uncut_local_rect(canvas(), partial).is_none());
        let within = RECT {
            left: -1400,
            top: -150,
            right: -1280,
            bottom: -130,
        };
        assert_eq!(uncut_local_rect(canvas(), within), Some(line_rect()));
        assert!(!contains_rect(
            Rect {
                width: 100,
                ..line_rect()
            },
            line_rect()
        ));
    }

    #[test]
    fn frozen_z_order_rejects_only_occluded_block_and_keeps_other_window_evidence() {
        let make = |hwnd, rect: Rect| WindowTarget {
            rect,
            title: String::new(),
            hwnd,
            process_id: 7,
            thread_id: 8,
            global_bounds: RECT::default(),
        };
        let above = make(
            1,
            Rect {
                x: 100,
                y: 40,
                width: 30,
                height: 40,
            },
        );
        let below = make(2, Rect::full(4000, 1640));
        let targets = [above, below];
        let context = RegionContext {
            canvas: canvas(),
            targets: &targets,
            window_index: 1,
            clip: Rect::full(4000, 1640),
        };
        assert!(!region_block_visible(&context, line_rect()));
        assert!(region_block_visible(
            &context,
            Rect {
                x: 40,
                y: 100,
                ..line_rect()
            }
        ));
        let top = RegionContext {
            window_index: 0,
            ..context
        };
        assert!(region_block_visible(
            &top,
            Rect {
                x: 105,
                y: 50,
                width: 20,
                height: 20
            }
        ));
    }

    #[test]
    fn incomplete_or_password_descendant_prevents_aggregate_but_safe_sibling_remains() {
        let mut parent = SubtreeRead {
            complete: true,
            protected: false,
            invalidated: false,
        };
        let safe = SubtreeRead {
            complete: true,
            protected: false,
            invalidated: false,
        };
        assert!(safe.may_read_container());
        parent.include(safe);
        parent.include(SubtreeRead {
            complete: true,
            protected: true,
            invalidated: false,
        });
        assert!(!parent.may_read_container());
        assert!(safe.may_read_container());
        let mut invalidated_parent = safe;
        invalidated_parent.include(SubtreeRead {
            complete: false,
            protected: true,
            invalidated: true,
        });
        assert!(invalidated_parent.invalidated);
        assert!(!invalidated_parent.may_read_container());
        let mut bounded_parent = safe;
        bounded_parent.include(SubtreeRead {
            complete: false,
            protected: false,
            invalidated: false,
        });
        assert!(!bounded_parent.may_read_container());
        let mut output = RegionOutput::default();
        assert!(output.add(line_rect(), "安全孩子"));
        assert_eq!(output.blocks.len(), 1);
        assert!(!bounded_parent.may_read_container());
        assert_eq!(output.blocks[0].text, "安全孩子");
        let subtree_start = output.blocks.len();
        let subtree_units = output.units;
        assert!(output.add(
            Rect {
                y: 80,
                ..line_rect()
            },
            "祖先变化下的孩子"
        ));
        let changed: Box<dyn std::error::Error> = Box::new(RegionAncestorChanged);
        assert!(changed.is::<RegionAncestorChanged>());
        output.rollback(subtree_start, subtree_units);
        assert_eq!(output.blocks.len(), 1);
        assert_eq!(output.blocks[0].text, "安全孩子");
        assert_eq!(output.units, subtree_units);
    }

    #[test]
    fn line_text_rejects_provider_document_upgrade_and_utf16_truncated_prefix() {
        assert_eq!(region_line_text(" 完整😀行\r\n"), Some("完整😀行"));
        for value in [
            "\r\n",
            "第一行\n第二行",
            "第一行\u{2028}第二行",
            "第一行\u{2029}第二行",
        ] {
            assert!(region_line_text(value).is_none());
        }
        assert!(region_line_text(&"😀".repeat(MAX_LINE_UNITS / 2)).is_some());
        assert!(region_line_text(&"😀".repeat(MAX_LINE_UNITS / 2 + 1)).is_none());
    }

    #[test]
    fn total_budget_keeps_whole_completed_blocks_without_truncated_text() {
        let mut output = RegionOutput::default();
        let first = "😀".repeat(MAX_TEXT_UNITS / 2 - 1);
        assert!(output.add(line_rect(), &first));
        assert!(!output.add(
            Rect {
                y: 80,
                ..line_rect()
            },
            "越界😀"
        ));
        assert_eq!(output.units, MAX_TEXT_UNITS - 2);
        assert_eq!(output.blocks.len(), 1);
        assert_eq!(output.blocks[0].text, first);
        assert!(output.add(
            Rect {
                y: 80,
                ..line_rect()
            },
            "ab"
        ));
        assert!(output.full());
    }

    #[test]
    fn duplicate_container_line_is_removed_but_distinct_row_and_whole_value_are_kept() {
        let mut output = RegionOutput::default();
        assert!(output.add(line_rect(), " 行😀 "));
        assert!(output.add(line_rect(), "行😀"));
        assert_eq!(output.blocks.len(), 1);
        assert!(output.add(
            Rect {
                y: 80,
                ..line_rect()
            },
            "行😀"
        ));
        assert!(output.add(
            Rect {
                height: 80,
                ..line_rect()
            },
            "值第一行\n值第二行"
        ));
        assert_eq!(output.blocks.len(), 3);
        assert_eq!(output.blocks[2].text, "值第一行\n值第二行");
        assert_eq!(output.blocks[2].rect.height, 80);
    }
}

#[cfg(test)]
mod text_source_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::fmt;

    type SourceResult = Result<Option<String>>;

    fn source<'a>(
        calls: &'a RefCell<Vec<&'static str>>,
        name: &'static str,
        result: SourceResult,
    ) -> impl FnOnce() -> SourceResult + 'a {
        move || {
            calls.borrow_mut().push(name);
            result
        }
    }

    fn select(text: SourceResult, value: SourceResult) -> (SourceResult, Vec<&'static str>) {
        let calls = RefCell::new(Vec::new());
        let result = element_text_with_sources(
            source(&calls, "TextPattern", text),
            source(&calls, "ValuePattern", value),
        );
        (result, calls.into_inner())
    }

    fn select_with_forbidden_value(text: SourceResult) -> SourceResult {
        let calls = RefCell::new(Vec::new());
        let result =
            element_text_with_sources(source(&calls, "TextPattern", text), || -> SourceResult {
                panic!("此路径禁止探测同元素 ValuePattern")
            });
        assert_eq!(calls.into_inner(), ["TextPattern"]);
        result
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FailureKind {
        Provider,
        Budget,
    }

    #[derive(Debug)]
    struct SourceFailure {
        kind: FailureKind,
        operation: &'static str,
    }

    impl fmt::Display for SourceFailure {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{:?}: {}", self.kind, self.operation)
        }
    }

    impl std::error::Error for SourceFailure {}

    fn failed_source(
        kind: FailureKind,
        operation: &'static str,
    ) -> (SourceResult, *const SourceFailure) {
        let error = Box::new(SourceFailure { kind, operation });
        let identity = &*error as *const SourceFailure;
        (Err(error), identity)
    }

    fn assert_original_error(
        result: SourceResult,
        identity: *const SourceFailure,
        kind: FailureKind,
        operation: &'static str,
    ) {
        let error = result.unwrap_err();
        let returned = error
            .downcast_ref::<SourceFailure>()
            .expect("来源错误必须保留具体类型");
        assert!(std::ptr::eq(returned, identity));
        assert_eq!(returned.kind, kind);
        assert_eq!(returned.operation, operation);
    }

    fn assert_length_error(result: SourceResult) {
        assert_eq!(
            result.unwrap_err().to_string(),
            "控件正文超过长度上限，使用选区 OCR"
        );
    }

    #[test]
    fn nonempty_text_does_not_probe_value() {
        for raw in ["正文", " \r\n正文\t ", "\u{200b}", "\u{feff}"] {
            assert_eq!(
                select_with_forbidden_value(Ok(Some(raw.into()))).unwrap(),
                Some(raw.into())
            );
        }
    }

    #[test]
    fn unsupported_text_reads_value_once_after_text() {
        let (result, calls) = select(Ok(None), Ok(Some("同元素值".into())));
        assert_eq!(result.unwrap(), Some("同元素值".into()));
        assert_eq!(calls, ["TextPattern", "ValuePattern"]);
    }

    #[test]
    fn empty_text_reads_value_once() {
        let (result, calls) = select(Ok(Some(String::new())), Ok(Some("有效值".into())));
        assert_eq!(result.unwrap(), Some("有效值".into()));
        assert_eq!(calls, ["TextPattern", "ValuePattern"]);
    }

    #[test]
    fn unicode_whitespace_text_reads_value_once() {
        for blank in [" \t\r\n", "\u{00a0}\u{2003}\u{3000}", "\u{2028}\u{2029}"] {
            let (result, calls) = select(Ok(Some(blank.into())), Ok(Some("有效值".into())));
            assert_eq!(result.unwrap(), Some("有效值".into()));
            assert_eq!(calls, ["TextPattern", "ValuePattern"]);
        }
    }

    #[test]
    fn maximum_blank_text_may_fall_back() {
        let blank = "\u{3000}".repeat(MAX_TEXT_UNITS);
        assert_eq!(blank.encode_utf16().count(), MAX_TEXT_UNITS);
        let (result, calls) = select(Ok(Some(blank)), Ok(Some("有效值".into())));
        assert_eq!(result.unwrap(), Some("有效值".into()));
        assert_eq!(calls, ["TextPattern", "ValuePattern"]);
    }

    #[test]
    fn unsupported_or_blank_both_sources_remain_none() {
        let absent = [None, Some(String::new()), Some(" \r\n\u{3000}".into())];
        for text in &absent {
            for value in &absent {
                let (result, calls) = select(Ok(text.clone()), Ok(value.clone()));
                assert_eq!(result.unwrap(), None);
                assert_eq!(calls, ["TextPattern", "ValuePattern"]);
            }
        }
    }

    #[test]
    fn text_preserves_raw_spaces_and_crlf() {
        let raw = " \t第一行😀\r\n第二行 e\u{0301}\r\n ";
        assert_eq!(
            select_with_forbidden_value(Ok(Some(raw.into()))).unwrap(),
            Some(raw.into())
        );
    }

    #[test]
    fn value_preserves_raw_spaces_and_crlf() {
        let raw = " \t值😀\r\n第二行 e\u{0301}\r\n ";
        for text in [None, Some(String::new()), Some(" \r\n".into())] {
            let (result, calls) = select(Ok(text), Ok(Some(raw.into())));
            assert_eq!(result.unwrap(), Some(raw.into()));
            assert_eq!(calls, ["TextPattern", "ValuePattern"]);
        }
    }

    #[test]
    fn ascii_text_utf16_boundary_is_exact() {
        for units in [MAX_TEXT_UNITS - 1, MAX_TEXT_UNITS] {
            let raw = "a".repeat(units);
            assert_eq!(raw.encode_utf16().count(), units);
            assert_eq!(
                select_with_forbidden_value(Ok(Some(raw.clone()))).unwrap(),
                Some(raw)
            );
        }
    }

    #[test]
    fn surrogate_text_utf16_boundary_is_exact() {
        let exact = "😀".repeat(MAX_TEXT_UNITS / 2);
        let below = format!("{}a", "😀".repeat(MAX_TEXT_UNITS / 2 - 1));
        for (raw, units) in [(below, MAX_TEXT_UNITS - 1), (exact, MAX_TEXT_UNITS)] {
            assert_eq!(raw.encode_utf16().count(), units);
            assert!(raw.chars().count() < MAX_TEXT_UNITS);
            assert_eq!(
                select_with_forbidden_value(Ok(Some(raw.clone()))).unwrap(),
                Some(raw)
            );
        }
    }

    #[test]
    fn combining_text_utf16_boundary_is_exact() {
        let raw = "e\u{0301}".repeat(MAX_TEXT_UNITS / 2);
        assert_eq!(raw.encode_utf16().count(), MAX_TEXT_UNITS);
        assert_eq!(
            select_with_forbidden_value(Ok(Some(raw.clone()))).unwrap(),
            Some(raw)
        );
    }

    #[test]
    fn ascii_text_over_limit_never_probes_value() {
        assert_length_error(select_with_forbidden_value(Ok(Some(
            "a".repeat(MAX_TEXT_UNITS + 1),
        ))));
    }

    #[test]
    fn surrogate_text_over_limit_never_probes_value() {
        let raw = format!("{}a", "😀".repeat(MAX_TEXT_UNITS / 2));
        assert_eq!(raw.encode_utf16().count(), MAX_TEXT_UNITS + 1);
        assert!(raw.chars().count() < MAX_TEXT_UNITS);
        assert_length_error(select_with_forbidden_value(Ok(Some(raw))));
    }

    #[test]
    fn whitespace_text_over_limit_never_probes_value() {
        let raw = "\u{3000}".repeat(MAX_TEXT_UNITS + 1);
        assert!(raw.trim().is_empty());
        assert_length_error(select_with_forbidden_value(Ok(Some(raw))));
    }

    #[test]
    fn text_provider_error_is_identical_and_never_probes_value() {
        let (source, identity) = failed_source(FailureKind::Provider, "DocumentRange/GetText");
        assert_original_error(
            select_with_forbidden_value(source),
            identity,
            FailureKind::Provider,
            "DocumentRange/GetText",
        );
    }

    #[test]
    fn text_budget_error_is_identical_and_never_probes_value() {
        let (source, identity) = failed_source(FailureKind::Budget, "checked TextPattern");
        assert_original_error(
            select_with_forbidden_value(source),
            identity,
            FailureKind::Budget,
            "checked TextPattern",
        );
    }

    #[test]
    fn value_provider_error_is_identical_after_fallback() {
        let (value, identity) = failed_source(FailureKind::Provider, "CurrentValue");
        let (result, calls) = select(Ok(Some(" \r\n".into())), value);
        assert_original_error(result, identity, FailureKind::Provider, "CurrentValue");
        assert_eq!(calls, ["TextPattern", "ValuePattern"]);
    }

    #[test]
    fn value_budget_error_is_identical_after_fallback() {
        let (value, identity) = failed_source(FailureKind::Budget, "checked ValuePattern");
        let (result, calls) = select(Ok(None), value);
        assert_original_error(
            result,
            identity,
            FailureKind::Budget,
            "checked ValuePattern",
        );
        assert_eq!(calls, ["TextPattern", "ValuePattern"]);
    }

    #[test]
    fn value_unicode_utf16_boundaries_are_exact() {
        for raw in [
            "a".repeat(MAX_TEXT_UNITS),
            "😀".repeat(MAX_TEXT_UNITS / 2),
            "e\u{0301}".repeat(MAX_TEXT_UNITS / 2),
        ] {
            assert_eq!(raw.encode_utf16().count(), MAX_TEXT_UNITS);
            let (result, calls) = select(Ok(None), Ok(Some(raw.clone())));
            assert_eq!(result.unwrap(), Some(raw));
            assert_eq!(calls, ["TextPattern", "ValuePattern"]);
        }
    }

    #[test]
    fn value_unicode_utf16_overflow_is_an_error() {
        for raw in [
            "a".repeat(MAX_TEXT_UNITS + 1),
            format!("{}a", "😀".repeat(MAX_TEXT_UNITS / 2)),
            format!("{}e", "e\u{0301}".repeat(MAX_TEXT_UNITS / 2)),
        ] {
            assert_eq!(raw.encode_utf16().count(), MAX_TEXT_UNITS + 1);
            let (result, calls) = select(Ok(None), Ok(Some(raw)));
            assert_length_error(result);
            assert_eq!(calls, ["TextPattern", "ValuePattern"]);
        }
    }

    #[test]
    fn whitespace_value_over_limit_is_an_error() {
        let raw = " ".repeat(MAX_TEXT_UNITS + 1);
        assert!(raw.trim().is_empty());
        let (result, calls) = select(Ok(None), Ok(Some(raw)));
        assert_length_error(result);
        assert_eq!(calls, ["TextPattern", "ValuePattern"]);
    }

    #[test]
    fn actual_checked_text_budget_stops_provider_and_value() {
        let budget = Budget::new(Duration::ZERO);
        let text_provider_calls = Cell::new(0);
        let value_probes = Cell::new(0);
        let result = element_text_with_sources(
            || {
                checked(&budget, || {
                    text_provider_calls.set(text_provider_calls.get() + 1);
                    Ok(Some("正文".into()))
                })
            },
            || {
                value_probes.set(value_probes.get() + 1);
                Ok(Some("有效值".into()))
            },
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "控件探测超过预算，回退窗口选区"
        );
        assert_eq!(text_provider_calls.get(), 0);
        assert_eq!(value_probes.get(), 0);
    }

    #[test]
    fn actual_checked_value_budget_stops_provider_after_blank_text() {
        let budget = Budget::new(Duration::ZERO);
        let calls = RefCell::new(Vec::new());
        let value_provider_calls = Cell::new(0);
        let result = element_text_with_sources(
            source(&calls, "TextPattern", Ok(Some(" \r\n".into()))),
            || {
                calls.borrow_mut().push("ValuePattern");
                checked(&budget, || {
                    value_provider_calls.set(value_provider_calls.get() + 1);
                    Ok(Some("有效值".into()))
                })
            },
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "控件探测超过预算，回退窗口选区"
        );
        assert_eq!(calls.into_inner(), ["TextPattern", "ValuePattern"]);
        assert_eq!(value_provider_calls.get(), 0);
    }
}
