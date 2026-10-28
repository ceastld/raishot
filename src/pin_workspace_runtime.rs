//! 工作区的窗口接入；磁盘只在内容/参数改变时更新，托盘不创建 GPUI/GPU。
use crate::{
    capture::Image,
    editor::Editor,
    geometry::Rect,
    gpui_capture::hwnd,
    pin_autosave::{ContentSchedule, SnapshotDecision},
    pin_model::{
        Adjustments, PinState, PinWindowSizing, Placement, ProcessingHistory,
        ProcessingHistorySnapshot,
    },
    pin_native,
    pin_text_document::WireTextDocument,
    pin_view::PinView,
    pin_workspace::{self as store, Action, PinEntry, PinStatus, Session},
    selection_finish::SelectionFinishStyle,
    Result,
};
use gpui::{App, Context, EntityId, Global, Window, WindowHandle};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    core::w,
    Win32::{Foundation::*, UI::WindowsAndMessaging::*},
};

const TICK: Duration = Duration::from_millis(250);
const RETRY: Duration = Duration::from_secs(1);

#[path = "pin_workspace_controls.rs"]
mod control_model;
#[path = "pin_workspace_control_runtime.rs"]
mod control_runtime;
#[cfg(test)]
pub(crate) use control_model::Identity;
pub(crate) use control_model::{InteractionAction, LiveControl, Outcome, Request};
#[cfg(test)]
pub(crate) use control_runtime::active_controls;
pub(crate) use control_runtime::{active_controls_summary, control_result, request_control};

/// 仅 crate 内专项可安装的一次性屏障；不持图、不改 busy，也不影响正常保存。
pub(crate) struct SnapshotPause {
    stage: AtomicU8,
    encode_released: AtomicBool,
    commit_released: AtomicBool,
}
impl SnapshotPause {
    pub(crate) fn new(pause_before_encode: bool) -> Arc<Self> {
        Arc::new(Self {
            stage: AtomicU8::new(0),
            encode_released: AtomicBool::new(!pause_before_encode),
            commit_released: AtomicBool::new(false),
        })
    }
    pub(crate) fn stage(&self) -> u8 {
        self.stage.load(Ordering::Acquire)
    }
    pub(crate) fn release_encode(&self) {
        self.encode_released.store(true, Ordering::Release);
    }
    pub(crate) fn release_commit(&self) {
        self.commit_released.store(true, Ordering::Release);
    }
    fn wait(&self, stage: u8, release: &AtomicBool) -> Result<()> {
        self.stage.store(stage, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(20);
        while !release.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return Err("工作区专项屏障超时，保存正常返回失败".into());
            }
            std::thread::park_timeout(Duration::from_millis(5));
        }
        Ok(())
    }
}

pub(crate) enum SnapshotPixels {
    Shared {
        image: Arc<gpui::RenderImage>,
        size: (u32, u32),
    },
    Output(Image),
}
impl SnapshotPixels {
    pub(crate) fn pixels(&self) -> Result<((u32, u32), &[u8])> {
        match self {
            Self::Shared { image, size } => {
                Ok((*size, image.as_bytes(0).ok_or("工作区共享底图缺少像素")?))
            }
            Self::Output(image) => Ok(((image.width, image.height), &image.bgra)),
        }
    }
}
/// 只复用全图且没有待合成内容的当前底图；撤销栈本身不改变可见像素。
fn shared_snapshot_allowed(
    editor: &Editor,
    source: &gpui::RenderImage,
    image: &gpui::RenderImage,
    size: (u32, u32),
) -> bool {
    let full = Rect::full(size.0, size.1);
    std::ptr::eq(source, image)
        && editor.canvas == full
        && editor.crop == Some(full)
        && editor.document.items().is_empty()
        && editor.document.crop_mask.is_none()
        && editor.lasso_draft.is_none()
        && editor.text_draft.is_none()
        && !editor.is_dragging()
        && !editor.has_pending_drawing()
}
/// 文字来源只认矢量纯色底画布；底图位图或含光标的画布不能还原为文字文档。
fn text_snapshot_source_allowed(
    pin_mode: bool,
    include_cursor: bool,
    screen_count: usize,
    solid: bool,
) -> bool {
    pin_mode && !include_cursor && screen_count == 1 && solid
}
pub(crate) fn snapshot_pixels(
    image: &Arc<gpui::RenderImage>,
    size: (u32, u32),
    shared: bool,
    output: impl FnOnce() -> Result<Image>,
) -> Result<SnapshotPixels> {
    if shared {
        Ok(SnapshotPixels::Shared {
            image: image.clone(),
            size,
        })
    } else {
        output().map(SnapshotPixels::Output)
    }
}

#[derive(Clone, PartialEq)]
struct ContentStamp {
    image_id: usize,
    original_id: usize,
    image_size: (u32, u32),
    original_size: (u32, u32),
    presentation_style: Option<SelectionFinishStyle>,
    vector_text: Option<bool>,
    // Capture来源的crop及光标开关不一定增加Document revision，必须另记采样状态。
    capture_selection: Option<(Option<Rect>, bool)>,
    canvas: Option<EntityId>,
    revision: u64,
    processing_history: Option<std::result::Result<ProcessingHistorySnapshot, String>>,
}
/// 不用像素或 current 的 ABA 判断两栈；完整快照最多64个小参数，不持有图像。
fn processing_history_stamp(
    history: &ProcessingHistory,
    current: Adjustments,
    vector_text: bool,
) -> Option<std::result::Result<ProcessingHistorySnapshot, String>> {
    (!vector_text).then(|| {
        history
            .bounded_snapshot(current)
            .map_err(|error| error.to_string())
    })
}
/// 非矢量的 canonical 图片只是预览，实际恢复仍用原图和这些参数。
fn workspace_saved_state(state: &PinState, vector_text: bool) -> PinState {
    let mut saved = state.clone();
    if vector_text {
        saved.adjustments = Default::default();
    }
    saved
}
#[derive(Clone, PartialEq)]
struct StateStamp {
    state: PinState,
    placement: Placement,
    window_sizing: PinWindowSizing,
    click_through: bool,
    presentation_style: Option<SelectionFinishStyle>,
}
fn state_save_due(
    decision: SnapshotDecision,
    saved: Option<&StateStamp>,
    current: &StateStamp,
) -> bool {
    decision == SnapshotDecision::Clean && saved != Some(current)
}

#[derive(Default)]
struct SnapshotLedger {
    encode_starts: AtomicU64,
    encode_finishes: AtomicU64,
    last_encoded_unix_ms: AtomicU64,
}
fn snapshot_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
/// 只保存计数/时刻，不延长图片、保存任务或窗口的生命周期。
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(crate) struct WorkspaceSnapshotProbe {
    pub busy: bool,
    pub pending_close: Option<&'static str>,
    pub snapshots_started: u64,
    pub snapshots_committed: u64,
    pub snapshot_failures: u64,
    pub encode_starts: u64,
    pub encode_finishes: u64,
    pub last_started_unix_ms: u64,
    pub last_encoded_unix_ms: u64,
    pub last_committed_unix_ms: u64,
    pub deferred_ticks: u64,
    pub quiet_ms: u64,
    pub max_delay_ms: u64,
}
pub(crate) struct LivePin {
    pub session: Session,
    pub entry: PinEntry,
    pub close_finished: bool,
    refresh_revision: u64,
    content: Option<ContentStamp>,
    state: Option<StateStamp>,
    busy: bool,
    pending_close: Option<PinStatus>,
    forget_on_close: bool,
    last_attempt: Option<Instant>,
    next_pause: Option<Arc<SnapshotPause>>,
    content_schedule: ContentSchedule<ContentStamp>,
    snapshot_ledger: Arc<SnapshotLedger>,
    snapshots_started: u64,
    snapshots_committed: u64,
    snapshot_failures: u64,
    last_started_unix_ms: u64,
    last_committed_unix_ms: u64,
    deferred_ticks: u64,
    controls: control_runtime::OwnerControls,
}

fn id_hash(id: &str) -> u64 {
    id.bytes()
        .fold(0xcbf29ce484222325, |h, byte| {
            (h ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
        .max(1)
}
/// 贴图管理在主界面贴图页；从托盘或贴图右键打开时只启动／切换主界面。
pub(crate) fn open_manager(_selected: Option<String>, _cx: &mut App) -> Result<()> {
    crate::settings_view::main_page::launch(crate::settings_view::main_page::MainPage::Pins)
}

#[derive(Default)]
struct RestoreState {
    running: bool,
}
impl Global for RestoreState {}
pub(crate) fn is_restoring(cx: &App) -> bool {
    cx.try_global::<RestoreState>()
        .is_some_and(|state| state.running)
}

/// 清单和图片解码在后台进行；只将窗口创建交回 GPUI 前台。
pub(crate) fn spawn_restore_active(cx: &mut App) {
    if is_restoring(cx) {
        return;
    }
    cx.set_global(RestoreState { running: true });
    let list = cx
        .background_executor()
        .spawn(async { store::restorable_active().map_err(|error| error.to_string()) });
    cx.spawn(async move |cx| {
        match list.await {
            Ok(entries) => {
                for entry in entries {
                    let id = entry.id;
                    let label = id.clone();
                    let decoded = cx
                        .background_executor()
                        .spawn(
                            async move { Session::claim(&id).map_err(|error| error.to_string()) },
                        )
                        .await;
                    match decoded {
                        Ok((session, image, entry)) => {
                            match cx.update(|cx| PinView::open_session(image, session, entry, cx)) {
                                Ok(Ok(_)) => {}
                                Ok(Err(error)) => {
                                    eprintln!("跳过尚不可恢复的贴图 {label}：{error}")
                                }
                                Err(_) => return,
                            }
                        }
                        Err(error) => eprintln!("跳过尚不可恢复的贴图 {label}：{error}"),
                    }
                }
            }
            Err(error) => eprintln!("工作区启动恢复失败：{error}"),
        }
        let _ = cx.update(|cx| {
            cx.set_global(RestoreState::default());
            if cx.windows().is_empty() && !crate::pin_live_view::has_open_windows() {
                cx.quit();
            }
        });
    })
    .detach();
}

/// 跨进程只传递受控 HWND 属性；GPUI 实体在其自身任务中消费通知。
pub(crate) fn windows() -> Vec<(String, HWND)> {
    let Ok(workspace) = store::load_summary() else {
        return Vec::new();
    };
    windows_summary(&workspace)
}
fn windows_in(workspace: &store::Workspace) -> Vec<(String, HWND)> {
    windows_with_ids(workspace.pins.iter().map(|pin| pin.id.as_str()))
}
pub(crate) fn windows_summary(workspace: &store::WorkspaceSummary) -> Vec<(String, HWND)> {
    windows_with_ids(workspace.pins.iter().map(|pin| pin.id.as_str()))
}
fn windows_with_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Vec<(String, HWND)> {
    struct Scan {
        scope: usize,
        ids: Vec<(usize, String)>,
        found: Vec<(String, HWND)>,
    }
    unsafe extern "system" fn visit(h: HWND, data: LPARAM) -> windows::core::BOOL {
        let scan = &mut *(data.0 as *mut Scan);
        if !GetPropW(h, w!("RustShot.Pin.v1")).is_invalid()
            && GetPropW(h, w!("RustShot.Pin.WorkspaceScope")).0 as usize == scan.scope
        {
            let key = GetPropW(h, w!("RustShot.Pin.WorkspaceId")).0 as usize;
            if let Some((_, id)) = scan.ids.iter().find(|(hash, _)| *hash == key) {
                scan.found.push((id.clone(), h));
            }
        }
        windows::core::BOOL(1)
    }
    let mut scan = Scan {
        scope: store::scope() as usize,
        ids: ids
            .map(|id| (id_hash(id) as usize, id.to_owned()))
            .collect(),
        found: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut scan as *mut _ as isize));
    }
    scan.found
}
pub(crate) fn manual_restore_ids(workspace: &store::Workspace) -> HashSet<String> {
    let mut ids = store::manual_restore_ids(workspace);
    for (id, _) in windows_in(workspace) {
        ids.remove(&id);
    }
    ids
}
pub(crate) fn manual_restore_summary_ids(workspace: &store::WorkspaceSummary) -> HashSet<String> {
    let mut ids = store::manual_restore_summary_ids(workspace);
    for (id, _) in windows_summary(workspace) {
        ids.remove(&id);
    }
    ids
}
pub(crate) fn broadcast() {
    let Ok(workspace) = store::load_summary() else {
        return;
    };
    // 接收窗口与版本来自同一清单快照，避免同步开窗期间再读一次磁盘。
    for (_, h) in windows_summary(&workspace) {
        unsafe {
            let _ = SetPropW(
                h,
                w!("RustShot.Pin.WorkspaceRefresh"),
                Some(HANDLE(workspace.revision.max(1) as usize as *mut _)),
            );
            let _ = PostMessageW(Some(h), WM_NULL, WPARAM(0), LPARAM(0));
        }
    }
}
pub(crate) fn preserve_on_exit(h: HWND) {
    if !unsafe { GetPropW(h, w!("RustShot.Pin.v1")) }.is_invalid() {
        unsafe {
            let _ = SetPropW(
                h,
                w!("RustShot.Pin.ExitPreserve"),
                Some(HANDLE(std::ptr::dangling_mut())),
            );
        }
    }
}
pub(crate) fn close_record(id: &str) -> Result<()> {
    if let Some((_, h)) = windows().into_iter().find(|(key, _)| key == id) {
        unsafe {
            PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0))?;
        }
    } else {
        if crate::preferences::Preferences::load().pin_restore {
            store::apply(Action::Close { id: id.into() })?;
        } else {
            store::forget(id)?;
        }
        broadcast();
    }
    Ok(())
}
pub(crate) fn restore_record(id: &str, cx: &mut App) -> Result<WindowHandle<PinView>> {
    if windows().iter().any(|(live_id, _)| live_id == id) {
        return Err(tr!(
            "贴图窗口仍在使用，无需重复恢复",
            "The pin window is still open; no restore is needed"
        )
        .into());
    }
    let (session, image, entry) = Session::claim(id)?;
    PinView::open_session(image, session, entry, cx)
}
pub(crate) fn restore_last_closed(cx: &mut App) -> Result<WindowHandle<PinView>> {
    if let Some(entry) = store::closed_current()?.into_iter().next() {
        return restore_record(&entry.id, cx);
    }
    if store::load()?.current_group != store::DEFAULT_GROUP_ID {
        return Err("当前分组没有可恢复的已关闭贴图".into());
    }
    let mut legacy = crate::pin_store::restore()?;
    let image = legacy.image.take().ok_or("旧贴图记录没有图片")?;
    let window = if let Some(document) = legacy.text_document.take() {
        // 兼容 PNG 不成为文字来源；只有成功安装完整文档后才消费关闭记录。
        drop(image);
        PinView::open_text_document_with_window(
            &document,
            legacy.origin,
            legacy.state.clone(),
            legacy.presentation_window,
            cx,
        )?
    } else if let Some(pixels) = legacy.capture_pixels.take() {
        let document = legacy
            .raster_document
            .take()
            .ok_or("截图恢复缺少完整文档")?;
        if legacy.processing_history.is_some() {
            return Err("截图完整来源不能混用旧处理历史".into());
        }
        PinView::open_restored_capture_document(
            image,
            pixels,
            document,
            (
                legacy.state.clone(),
                legacy.origin,
                legacy.presentation_window,
            ),
            legacy.presentation_style,
            cx,
        )?
    } else {
        match (
            legacy.presentation_style,
            legacy.raster_document.take(),
            legacy.presentation_window,
        ) {
            (style, Some(document), Some(window)) => PinView::open_closed_raster_document(
                image,
                legacy.state.clone(),
                style,
                document,
                window,
                legacy.processing_history.take(),
                cx,
            )?,
            (style, Some(document), None) => PinView::open_raster_document(
                image,
                legacy.origin,
                legacy.state.clone(),
                style,
                document,
                legacy.processing_history.take(),
                cx,
            )?,
            (None, None, None) => {
                PinView::open_state(image, legacy.origin, legacy.state.clone(), cx)?
            }
            _ => return Err("旧贴图记录的截图外观与文档不完整".into()),
        }
    };
    // 旧记录只有成功创建窗口后才消费；新工作区先保存首次 canonical 图片。
    legacy.commit()?;
    Ok(window)
}
/// 只切换本实例当前分组的工作区窗口；未登记窗口不借兼容入口参与操作。
pub(crate) fn toggle_current_group(owners: &[u32]) -> Result<usize> {
    let count = store::toggle_current_group(|workspace| {
        windows_in(workspace)
            .into_iter()
            .filter_map(|(id, window)| {
                let mut owner = 0;
                unsafe {
                    GetWindowThreadProcessId(window, Some(&mut owner));
                }
                (owner != 0 && owners.contains(&owner)).then_some(id)
            })
            .collect()
    })?;
    if count > 0 {
        broadcast();
    }
    Ok(count)
}

/// 分组顺序和当前组在同一次工作区事务中读取；沿用分组切换的窗口同步。
pub(crate) fn cycle_group(forward: bool) -> Result<String> {
    let workspace = store::cycle_group(forward)?;
    let name = workspace
        .groups
        .iter()
        .find(|group| group.id == workspace.current_group)
        .map(|group| group.display_name())
        .ok_or(tr!("分组不存在", "The group doesn't exist"))?;
    if workspace.groups.len() > 1 {
        broadcast();
    }
    Ok(name)
}

/// 托盘仅操作本实例持有且属于当前组的实时窗口；手动隐藏写入持久状态。
pub(crate) fn manage(owners: &[u32], action: pin_native::Manage) -> Result<usize> {
    let workspace = store::load()?;
    let targets: Vec<_> = windows()
        .into_iter()
        .filter(|(id, h)| {
            let mut pid = 0;
            unsafe {
                GetWindowThreadProcessId(*h, Some(&mut pid));
            }
            owners.contains(&pid)
                && workspace.pins.iter().any(|pin| {
                    pin.id == *id
                        && (matches!(action, pin_native::Manage::Close)
                            || pin.group_id == workspace.current_group)
                        && pin.status == PinStatus::Active
                })
        })
        .collect();
    let result = (|| -> Result<usize> {
        for (id, h) in &targets {
            match action {
                pin_native::Manage::Close => unsafe {
                    PostMessageW(Some(*h), WM_CLOSE, WPARAM(0), LPARAM(0))?;
                },
                pin_native::Manage::Hide
                | pin_native::Manage::Show
                | pin_native::Manage::Interact => {
                    store::apply(Action::SetHidden {
                        id: id.clone(),
                        hidden: matches!(action, pin_native::Manage::Hide),
                    })?;
                    if matches!(action, pin_native::Manage::Interact) {
                        pin_native::click_through(*h, false)?;
                    }
                }
            }
        }
        Ok(targets.len())
    })();
    broadcast();
    result
}

impl PinView {
    pub(crate) fn workspace_snapshot_probe(&self) -> Option<WorkspaceSnapshotProbe> {
        self.workspace.as_ref().map(|live| WorkspaceSnapshotProbe {
            busy: live.busy,
            pending_close: live.pending_close.map(|status| match status {
                PinStatus::Active => "active",
                PinStatus::Closed => "closed",
            }),
            snapshots_started: live.snapshots_started,
            snapshots_committed: live.snapshots_committed,
            snapshot_failures: live.snapshot_failures,
            encode_starts: live.snapshot_ledger.encode_starts.load(Ordering::Acquire),
            encode_finishes: live.snapshot_ledger.encode_finishes.load(Ordering::Acquire),
            last_started_unix_ms: live.last_started_unix_ms,
            last_encoded_unix_ms: live
                .snapshot_ledger
                .last_encoded_unix_ms
                .load(Ordering::Acquire),
            last_committed_unix_ms: live.last_committed_unix_ms,
            deferred_ticks: live.deferred_ticks,
            quiet_ms: crate::pin_autosave::QUIET.as_millis() as u64,
            max_delay_ms: crate::pin_autosave::MAX_DELAY.as_millis() as u64,
        })
    }
    /// 受控验收读取实际保存状态；不延长快照生命周期或改变后台任务。
    pub(crate) fn workspace_snapshot_busy(&self) -> bool {
        self.workspace.as_ref().is_some_and(|live| live.busy)
    }
    pub(crate) fn pause_next_workspace_snapshot_for_qa(
        &mut self,
        pause: Arc<SnapshotPause>,
    ) -> Result<()> {
        let live = self.workspace.as_mut().ok_or("专项贴图尚未登记工作区")?;
        if live.next_pause.is_some() {
            return Err("专项保存屏障尚未消费".into());
        }
        live.next_pause = Some(pause);
        Ok(())
    }

    pub(crate) fn attach_workspace(
        &mut self,
        restored: Option<(Session, PinEntry)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let placement = pin_native::rect(hwnd(window))?;
        let (session, entry, restored_content) = match restored {
            Some((session, entry)) => (session, entry, true),
            None => {
                let (session, entry) = Session::register_with_entry(
                    self.state.clone(),
                    placement,
                    self.window_sizing,
                )?;
                (session, entry, false)
            }
        };
        let native = hwnd(window);
        let controls = control_runtime::OwnerControls::new(restored_content && entry.click_through);
        unsafe {
            SetPropW(
                native,
                w!("RustShot.Pin.WorkspaceScope"),
                Some(HANDLE(store::scope() as usize as *mut _)),
            )?;
            SetPropW(
                native,
                w!("RustShot.Pin.WorkspaceId"),
                Some(HANDLE(id_hash(&session.id) as usize as *mut _)),
            )?;
        }
        self.workspace = Some(LivePin {
            session,
            entry,
            close_finished: false,
            refresh_revision: 0,
            content: restored_content.then(|| self.workspace_content(cx)),
            state: None,
            busy: false,
            pending_close: None,
            forget_on_close: false,
            last_attempt: None,
            next_pause: None,
            content_schedule: ContentSchedule::default(),
            snapshot_ledger: Arc::default(),
            snapshots_started: 0,
            snapshots_committed: 0,
            snapshot_failures: 0,
            last_started_unix_ms: 0,
            last_committed_unix_ms: 0,
            deferred_ticks: 0,
            controls,
        });
        self.refresh_workspace(window, cx)?;
        if restored_content {
            let live = self.workspace.as_mut().ok_or("恢复贴图登记缺失")?;
            live.session.mark_active()?;
            live.entry.status = PinStatus::Active;
        }
        self.workspace_controls_attach(window, cx)?;
        broadcast();
        cx.spawn_in(window, async move |view, cx| loop {
            cx.background_executor().timer(TICK).await;
            if view
                .update_in(cx, |v, window, cx| v.workspace_tick(window, cx))
                .is_err()
            {
                break;
            }
        })
        .detach();
        Ok(())
    }
    fn workspace_content(&self, cx: &App) -> ContentStamp {
        ContentStamp {
            // 独占 Frame 被消费再创建后，Arc 地址可重用，ImageId 才是内容身份。
            image_id: self.image.id.0,
            original_id: self.original.id.0,
            image_size: self.image_size,
            original_size: self.original_size,
            presentation_style: self.presentation_style,
            vector_text: self.vector_text,
            capture_selection: self.capture_source.as_ref().and_then(|_| {
                self.canvas.as_ref().map(|canvas| {
                    let view = canvas.read(cx);
                    (view.editor.crop, view.include_cursor)
                })
            }),
            canvas: self.canvas.as_ref().map(|c| c.entity_id()),
            revision: self
                .canvas
                .as_ref()
                .map_or(0, |c| c.read(cx).editor.document.revision()),
            processing_history: processing_history_stamp(
                &self.history,
                self.state.adjustments,
                self.vector_text.is_some(),
            ),
        }
    }
    fn refresh_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Result<()> {
        let workspace = store::load()?;
        let Some(live) = &mut self.workspace else {
            return Ok(());
        };
        if let Some(entry) = workspace.pins.into_iter().find(|p| p.id == live.session.id) {
            live.entry = entry;
        }
        let show = !live.entry.hidden && live.entry.group_id == workspace.current_group;
        window.set_window_title(&format!(
            "{} · {}",
            crate::brand::NAME,
            crate::pin_workspace::display_pin_title(&live.entry.title)
        ));
        if !show {
            crate::pin_shadow::hide_native(hwnd(window));
            self.set_canvas_active(false, window, cx)?;
            self.close_toolbar(cx);
            self.chrome = false;
        }
        unsafe {
            let h = hwnd(window);
            if !pin_native::defer_initial_visibility(h, show)?
                && IsWindowVisible(h).as_bool() != show
            {
                let _ = ShowWindowAsync(h, if show { SW_SHOWNOACTIVATE } else { SW_HIDE });
            }
        }
        Ok(())
    }
    fn workspace_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let h = hwnd(window);
        // 命令仅由窗口外层状态消费；原生回调仍只负责唤醒。
        let restoring_interaction = self.workspace_controls_tick(window, cx);
        let revision =
            unsafe { GetPropW(h, w!("RustShot.Pin.WorkspaceRefresh")) }.0 as usize as u64;
        if self
            .workspace
            .as_ref()
            .is_some_and(|p| p.refresh_revision != revision)
        {
            if let Err(error) = self.refresh_workspace(window, cx) {
                self.status = trf!(
                    "工作区刷新失败：{error}",
                    "Workspace refresh failed: {error}"
                );
            } else if let Some(live) = &mut self.workspace {
                live.refresh_revision = revision;
            }
        }
        // 工作区改名经刷新回到本窗；标题条在此按新标题、主题与实际透明度重画。
        self.pin_title_tick(window, cx);
        if self
            .workspace
            .as_ref()
            .is_some_and(|live| live.pending_close.is_some() && !live.busy)
            && !self.processing_blocks_close()
        {
            self.finish_pending_close(window, cx);
            return;
        }
        if restoring_interaction {
            return;
        }
        if self.saving
            || self.processing
            || self.gesture.is_some()
            || self.canvas.as_ref().is_some_and(|c| {
                let view = c.read(cx);
                let editor = &view.editor;
                editor.is_dragging()
                    || editor.has_pending_drawing()
                    || editor.text_draft.is_some()
                    || editor.watermark.has_preview()
            })
        {
            if let Some(live) = &mut self.workspace {
                live.content_schedule.activity(Instant::now());
            }
            return;
        }
        let Ok(placement) = pin_native::rect(h) else {
            return;
        };
        let state = StateStamp {
            state: self.state.clone(),
            placement,
            window_sizing: self.window_sizing,
            click_through: pin_native::transparent(h),
            presentation_style: self.presentation_style,
        };
        let content = self.workspace_content(cx);
        if let Some(Err(error)) = &content.processing_history {
            self.workspace_snapshot_error(error.clone(), cx);
            return;
        }
        let Some(live) = &mut self.workspace else {
            return;
        };
        // busy/重试期间仍观察最新稳定身份；旧 ACK 不得重置新内容的期限。
        let decision =
            live.content_schedule
                .observe(&content, live.content.as_ref(), Instant::now());
        if decision == SnapshotDecision::Deferred {
            live.deferred_ticks += 1;
        }
        if live.busy || live.last_attempt.is_some_and(|t| t.elapsed() < RETRY) {
            return;
        }
        if decision.due() {
            self.workspace_snapshot(window, cx, PinStatus::Active, false);
        } else if state_save_due(decision, live.state.as_ref(), &state) {
            let id = live.session.id.clone();
            let canonical = workspace_saved_state(&self.state, self.vector_text.is_some());
            match store::update_state_with_sizing(&id, &canonical, placement, state.window_sizing)
                .and_then(|()| {
                    live.session
                        .persist_interaction(self.state.locked, state.click_through)
                }) {
                Ok(()) => {
                    if let Some(live) = &mut self.workspace {
                        live.entry.state.locked = self.state.locked;
                        live.entry.window_sizing = state.window_sizing;
                        live.entry.click_through = state.click_through;
                        live.state = Some(state);
                    }
                }
                Err(error) => {
                    self.status = trf!(
                        "贴图位置保存失败：{error}",
                        "Failed to save the pin position: {error}"
                    );
                    if let Some(live) = &mut self.workspace {
                        live.last_attempt = Some(Instant::now());
                    }
                }
            }
        }
    }
    pub(crate) fn workspace_hide(&mut self, cx: &mut Context<Self>) -> Result<()> {
        self.finish_canvas_content(cx)?;
        if let Some(live) = &self.workspace {
            store::apply(Action::SetHidden {
                id: live.session.id.clone(),
                hidden: true,
            })?;
            broadcast();
        }
        self.close_toolbar(cx);
        self.chrome = false;
        Ok(())
    }
    /// 普通关闭写 Closed；程序退出写 Active。后台提交完成前保留窗口和 lease。
    pub(crate) fn workspace_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(live) = &self.workspace else {
            return true;
        };
        if live.close_finished {
            return true;
        }
        if self.saving {
            return false;
        }
        let preserve =
            !unsafe { GetPropW(hwnd(window), w!("RustShot.Pin.ExitPreserve")) }.is_invalid();
        let status = if preserve {
            PinStatus::Active
        } else {
            PinStatus::Closed
        };
        if let Err(error) = self.set_canvas_active(false, window, cx) {
            self.status = error.to_string();
            cx.notify();
            return false;
        }
        self.saving = true;
        self.close_toolbar(cx);
        self.chrome = false;
        let processing_blocks_close = self.processing_blocks_close();
        if let Some(live) = &mut self.workspace {
            live.pending_close = Some(status);
            live.forget_on_close =
                status == PinStatus::Closed && !crate::preferences::Preferences::load().pin_restore;
            if live.busy || processing_blocks_close {
                return false;
            }
        }
        self.finish_pending_close(window, cx);
        false
    }
    fn finish_pending_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(live) = &self.workspace else {
            return;
        };
        let Some(status) = live.pending_close else {
            return;
        };
        if live.forget_on_close {
            match live.session.forget() {
                Ok(()) => {
                    if let Some(live) = &mut self.workspace {
                        live.close_finished = true;
                    }
                    window.remove_window();
                    broadcast();
                }
                Err(error) => self.workspace_snapshot_error(error.to_string(), cx),
            }
        } else {
            self.workspace_snapshot(window, cx, status, true);
        }
    }
    /// 再贴一张使用同一矢量文字合同；不提交草稿或修改当前窗口。
    pub(crate) fn text_document_snapshot(&self, cx: &App) -> Result<Option<WireTextDocument>> {
        let Some(light) = self.vector_text else {
            return Ok(None);
        };
        if self.processing || self.state.adjustments != Default::default() {
            return Err("纯文字贴图尚在处理或带有不支持的图像参数".into());
        }
        let v = self
            .canvas
            .as_ref()
            .ok_or("纯文字贴图缺少矢量画布")?
            .read(cx);
        if !text_snapshot_source_allowed(
            v.pin_mode,
            v.include_cursor,
            v.source.screen_count(),
            v.source.solid_fill().is_some(),
        ) {
            return Err("纯文字贴图来源已失效，未降级为图片记录".into());
        }
        WireTextDocument::try_from_vector_text_editor(&v.editor, self.image_size, light).map(Some)
    }
    fn workspace_snapshot(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        status: PinStatus,
        finalize: bool,
    ) {
        let result = (|| -> Result<_> {
            // 与原 output 合同一致：先提交有效草稿，失败时不发布快照。
            if self.finish_canvas_content(cx)? {
                self.redo_edits.clear();
            }
            // 包含“切穿透后立即关闭”及编码期间参数变化的最终快照；
            // 保存失败仍沿原关闭合同保留窗口，不能丢失原生交互状态。
            let click_through = self.workspace_interaction_click_through(window);
            if let Some(live) = &mut self.workspace {
                live.session
                    .persist_interaction(self.state.locked, click_through)?;
                live.entry.state.locked = self.state.locked;
                live.entry.click_through = click_through;
            }
            let shared = self.canvas.as_ref().is_none_or(|canvas| {
                canvas.update(cx, |v, _| {
                    v.flush_tool_presets();
                    v.pin_mode
                        && !v.include_cursor
                        && v.source.screen_count() == 1
                        && v.previews.first().is_some_and(|preview| {
                            shared_snapshot_allowed(
                                &v.editor,
                                v.source.original(preview),
                                &self.image,
                                self.image_size,
                            )
                        })
                })
            });
            let text_document = self.text_document_snapshot(cx)?;
            let current = self.image.clone();
            let raster = if self.vector_text.is_none() {
                let document = self.raster_document_snapshot(cx)?;
                let metadata = store::RasterSnapshotMetadata {
                    original_size: self.original_size,
                    presentation_style: self.presentation_style,
                    document,
                    processing_history: self.processing_history_snapshot()?,
                };
                Some((
                    metadata,
                    SnapshotPixels::Shared {
                        image: self.original.clone(),
                        size: self.original_size,
                    },
                    self.capture_pixels.clone(),
                ))
            } else {
                None
            };
            let image = snapshot_pixels(&current, self.image_size, shared, || {
                if raster.is_some() {
                    // canonical保存完整内容画布；crop/mask仍留在文档，外观和窗口alpha不进像素。
                    let full = self.content_rect();
                    self.canvas
                        .as_ref()
                        .ok_or("截图贴图合成缺少画布")?
                        .update(cx, |v, _| v.content_image_in(full))
                } else {
                    self.output(cx)
                }
            })?;
            Ok((
                image,
                pin_native::rect(hwnd(window))?,
                text_document,
                raster,
            ))
        })();
        let (image, placement, text_document, raster) = match result {
            Ok(data) => data,
            Err(error) => {
                if let Some(live) = &mut self.workspace {
                    live.snapshot_failures += 1;
                }
                self.workspace_snapshot_error(error.to_string(), cx);
                return;
            }
        };
        let content = self.workspace_content(cx);
        // canonical 图片已包含所有图像处理及标注；恢复时不重复处理。
        let saved_state = StateStamp {
            state: self.state.clone(),
            placement,
            window_sizing: self.window_sizing,
            click_through: self.workspace_interaction_click_through(window),
            presentation_style: self.presentation_style,
        };
        let state = workspace_saved_state(&self.state, self.vector_text.is_some());
        let window_sizing = saved_state.window_sizing;
        let Some(live) = &mut self.workspace else {
            return;
        };
        let id = live.session.id.clone();
        let pause = live.next_pause.take();
        live.busy = true;
        live.snapshots_started += 1;
        live.last_started_unix_ms = snapshot_unix_ms();
        let ledger = live.snapshot_ledger.clone();
        live.last_attempt = Some(Instant::now());
        let task = cx.background_executor().spawn(async move {
            let save = || -> Result<()> {
                let size = image.pixels()?.0;
                let encode_current = move |path: &std::path::Path| {
                    if let Some(pause) = &pause {
                        pause.wait(1, &pause.encode_released)?;
                    }
                    ledger.encode_starts.fetch_add(1, Ordering::Release);
                    let encoded = image.pixels().and_then(|((width, height), bgra)| {
                        crate::export::save_png_pixels(width, height, bgra, path)
                    });
                    // 编码不再读像素后立即归还共享/合成帧，磁盘同步与清单提交无需整图。
                    drop(image);
                    ledger
                        .last_encoded_unix_ms
                        .store(snapshot_unix_ms(), Ordering::Release);
                    ledger.encode_finishes.fetch_add(1, Ordering::Release);
                    encoded?;
                    if let Some(pause) = &pause {
                        pause.wait(2, &pause.commit_released)?;
                    }
                    Ok(())
                };
                if let Some((metadata, original, context)) = raster {
                    let source = metadata.document.capture_source().cloned();
                    store::save_raster_snapshot_with_encoders(
                        &id,
                        &state,
                        (placement, window_sizing),
                        status,
                        metadata,
                        encode_current,
                        move |path| {
                            let result = original.pixels().and_then(|((width, height), bgra)| {
                                match (&source, &context) {
                                    (Some(source), Some(context)) => crate::pin_raster_document::capture_source::resource::save_png_bytes_with_context(
                                        (width, height), bgra, source, context, path,
                                    ),
                                    (None, None) => crate::export::save_png_pixels(width, height, bgra, path),
                                    _ => Err("工作区截图来源与CPU资源不匹配".into()),
                                }
                            });
                            drop(original);
                            result
                        },
                    )
                } else {
                    store::save_snapshot_with_text_document(
                        &id,
                        size,
                        &state,
                        (placement, window_sizing),
                        status,
                        text_document,
                        encode_current,
                    )
                }
            };
            save().map_err(|e| e.to_string())
        });
        cx.spawn_in(window, async move |view, cx| {
            let result = task.await;
            let _ = view.update_in(cx, |v, window, cx| {
                let Some(live) = &mut v.workspace else {
                    return;
                };
                live.busy = false;
                match result {
                    Ok(()) => {
                        live.snapshots_committed += 1;
                        live.last_committed_unix_ms = snapshot_unix_ms();
                        live.content_schedule.acknowledge(&content);
                        live.content = Some(content);
                        // 记录取快照时的状态，编码期间发生的新参数由下一轮 tick 补存。
                        // 快照只提交 PinState；穿透是原生状态，另由 owner 补存。
                        live.state = (live.entry.click_through == saved_state.click_through)
                            .then_some(saved_state);
                        let closing = live.pending_close;
                        if finalize && closing == Some(status) {
                            live.close_finished = true;
                            window.remove_window();
                        } else if closing.is_some() && !v.processing_blocks_close() {
                            // 自动快照是在关闭请求前取的；必须再取新快照包含最终编辑和参数。
                            v.finish_pending_close(window, cx);
                        }
                        broadcast();
                    }
                    Err(_) if live.forget_on_close && live.pending_close.is_some() => {
                        live.snapshot_failures += 1;
                        // 禁用关闭恢复时，旧自动快照失败也不阻止删除已有敏感记录。
                        v.finish_pending_close(window, cx);
                    }
                    Err(error) => {
                        live.snapshot_failures += 1;
                        v.workspace_snapshot_error(error, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn workspace_snapshot_error(&mut self, error: String, cx: &mut Context<Self>) {
        if let Some(live) = &mut self.workspace {
            live.busy = false;
            live.pending_close = None;
            live.last_attempt = Some(Instant::now());
        }
        self.saving = false;
        self.status = trf!(
            "工作区保存失败：{error}；窗口保留，可重试关闭。 ",
            "Workspace save failed: {error}; the window is kept and you can retry closing it. "
        );
        eprintln!("{}", self.status);
        cx.notify();
    }
}

#[cfg(test)]
mod shared_snapshot_tests {
    use super::*;

    #[test]
    fn captured_workspace_crop_and_cursor_change_invalidate_snapshot_without_document_revision_change(
    ) {
        let initial = ContentStamp {
            image_id: 1,
            original_id: 1,
            image_size: (96, 72),
            original_size: (44, 38),
            presentation_style: None,
            vector_text: None,
            capture_selection: Some((
                Some(Rect {
                    x: 15,
                    y: 11,
                    width: 52,
                    height: 46,
                }),
                true,
            )),
            canvas: None,
            revision: 7,
            processing_history: None,
        };
        let start = Instant::now();
        for selection in [
            Some((initial.capture_selection.unwrap().0, false)),
            Some((
                Some(Rect {
                    x: 16,
                    y: 11,
                    width: 51,
                    height: 46,
                }),
                true,
            )),
        ] {
            let changed = ContentStamp {
                capture_selection: selection,
                ..initial.clone()
            };
            let mut schedule = ContentSchedule::default();
            assert_eq!(
                schedule.observe(&initial, Some(&initial), start),
                SnapshotDecision::Clean
            );
            assert_eq!(
                schedule.observe(&changed, Some(&initial), start),
                SnapshotDecision::Deferred
            );
            assert_eq!(changed.revision, initial.revision);
        }
    }
    use crate::{
        annotation::{Annotation, Point, Style, Tool},
        pin_model::Adjustments,
        scene::{PixelPlane, Scene, SourcePixels},
    };

    #[test]
    fn processing_state_only_save_keeps_image_parameters_and_vector_legacy_reset() {
        let state = PinState {
            zoom: 1.75,
            opacity: 128,
            adjustments: Adjustments {
                turns: 1,
                brightness: 23,
                ..Default::default()
            },
            ..Default::default()
        };
        let actual = workspace_saved_state(&state, false);
        assert_eq!(actual, state, "无外观普通图片移动/透明度保存不清处理参数");
        let wire = crate::pin_model::WireProcessingHistory::from_history(
            &ProcessingHistory::default(),
            state.adjustments,
            (3, 2),
            (2, 3),
        )
        .unwrap();
        wire.validate(actual.adjustments, (3, 2), (2, 3)).unwrap();
        let text = workspace_saved_state(&state, true);
        assert_eq!(text.adjustments, Adjustments::default());
        assert_eq!((text.zoom, text.opacity), (state.zoom, state.opacity));
    }

    #[test]
    fn processing_history_stamp_detects_flip_rotate_aba_when_pixels_and_current_return() {
        let mut current = Adjustments::default();
        let mut history = ProcessingHistory::default();
        let initial = ContentStamp {
            image_id: 1,
            original_id: 1,
            image_size: (3, 2),
            original_size: (3, 2),
            presentation_style: None,
            vector_text: None,
            capture_selection: None,
            canvas: None,
            revision: 0,
            processing_history: processing_history_stamp(&history, current, false),
        };
        let mut schedule = ContentSchedule::default();
        let start = Instant::now();
        assert_eq!(
            schedule.observe(&initial, Some(&initial), start),
            SnapshotDecision::Clean
        );
        for flip_x in [true, false] {
            assert!(history.change(
                &mut current,
                Adjustments {
                    flip_x,
                    ..Default::default()
                }
            ));
        }
        assert_eq!(current, Adjustments::default());
        let flipped = ContentStamp {
            processing_history: processing_history_stamp(&history, current, false),
            ..initial.clone()
        };
        assert!(initial != flipped);
        assert_eq!(
            schedule.observe(&flipped, Some(&initial), start),
            SnapshotDecision::Deferred
        );
        assert_eq!(
            schedule.observe(&flipped, Some(&initial), start + crate::pin_autosave::QUIET),
            SnapshotDecision::Quiet
        );
        for _ in 0..4 {
            let next = current.rotated(1);
            assert!(history.change(&mut current, next));
        }
        assert_eq!(current, Adjustments::default());
        let rotated = ContentStamp {
            processing_history: processing_history_stamp(&history, current, false),
            ..initial.clone()
        };
        assert!(initial != rotated && flipped != rotated);
        assert_eq!(
            schedule.observe(&rotated, Some(&flipped), start + crate::pin_autosave::QUIET),
            SnapshotDecision::Deferred
        );
        assert!(
            processing_history_stamp(
                &history,
                Adjustments {
                    brightness: 101,
                    ..current
                },
                false
            )
            .unwrap()
            .is_err(),
            "坏语义保留Err交宿主传播，不伪装成未变"
        );
        assert!(
            processing_history_stamp(&history, current, true).is_none(),
            "纯文字不借图像处理两栈改变来源"
        );
    }

    #[test]
    fn mixed_text_theme_background_invalidates_autosave_without_current_text() {
        let full = Rect::full(8, 8);
        for marked in [false, true] {
            let mut editor = Editor::new(full, Some(full));
            if marked {
                let mut rectangle = Annotation::new(
                    Tool::Rectangle,
                    Point { x: 2., y: 2. },
                    Style {
                        color: 0xff0000,
                        filled: true,
                        radius: 0.,
                        ..Default::default()
                    },
                );
                rectangle.update(Point { x: 6., y: 6. });
                editor.document.insert(rectangle);
            }
            let revision = editor.document.revision();
            let items = editor.document.items().to_vec();
            let before = ContentStamp {
                image_id: 1,
                original_id: 1,
                image_size: (8, 8),
                original_size: (8, 8),
                presentation_style: None,
                vector_text: Some(true),
                capture_selection: None,
                canvas: None,
                revision,
                processing_history: None,
            };
            editor.document.remap_silently(|annotation| {
                crate::pin_text_source::remap_theme(annotation, true, false)
            });
            assert_eq!(editor.document.revision(), revision);
            assert_eq!(editor.document.items(), items);
            let after = ContentStamp {
                vector_text: Some(false),
                ..before.clone()
            };
            assert!(before != after, "主题纯色底改变必须使内容缓存失效");
            assert_ne!(
                crate::pin_text_source::fill(true),
                crate::pin_text_source::fill(false)
            );
            let mut schedule = ContentSchedule::default();
            let start = Instant::now();
            assert_eq!(
                schedule.observe(&before, Some(&before), start),
                SnapshotDecision::Clean
            );
            assert_eq!(
                schedule.observe(&after, Some(&before), start),
                SnapshotDecision::Deferred
            );
            assert_eq!(
                schedule.observe(&after, Some(&before), start + crate::pin_autosave::QUIET),
                SnapshotDecision::Quiet
            );
            assert_eq!(
                schedule.observe(&after, Some(&after), start + crate::pin_autosave::QUIET),
                SnapshotDecision::Clean
            );
        }
    }

    #[test]
    fn pin_workspace_text_document_requires_solid_vector_source_without_overlay() {
        let full = Rect::full(8, 8);
        let image = crate::pin_view::preview(Image {
            width: 8,
            height: 8,
            bgra: vec![255; 8 * 8 * 4],
            readback_ms: 0.,
        })
        .unwrap();
        let mut editor = Editor::new(full, Some(full));
        let mut text = Annotation::new(Tool::Text, Point { x: 1., y: 1. }, Style::default());
        text.update(Point { x: 7., y: 7. });
        text.text = "文字".into();
        editor.document.insert(text);
        assert!(text_snapshot_source_allowed(true, false, 1, true));
        assert!(WireTextDocument::from_text_editor(&editor, (8, 8), true).is_some());
        assert!(!shared_snapshot_allowed(&editor, &image, &image, (8, 8)));
        assert!(!text_snapshot_source_allowed(true, true, 1, true));
        assert!(!text_snapshot_source_allowed(false, false, 1, true));
        assert!(!text_snapshot_source_allowed(true, false, 2, true));
        // 位图底（含旧白底位图）不再被识别为文字文档。
        assert!(!text_snapshot_source_allowed(true, false, 1, false));
    }

    #[test]
    fn pin_workspace_shared_snapshot_reuses_processed_pixels_without_rendering() {
        let source = Image {
            width: 2,
            height: 2,
            bgra: vec![
                8, 32, 96, 128, 16, 64, 128, 255, 0, 0, 0, 0, 48, 80, 112, 192,
            ],
            readback_ms: 0.,
        };
        let processed = Adjustments {
            brightness: 10,
            flip_x: true,
            ..Default::default()
        }
        .apply(&source)
        .unwrap();
        let expected = [
            41, 89, 153, 255, 33, 57, 121, 128, 73, 105, 137, 192, 25, 25, 25, 0,
        ];
        assert_eq!(processed.bgra, expected);
        let image = crate::pin_view::preview(processed).unwrap();
        let full = Rect::full(2, 2);
        let mut editor = Editor::new(full, Some(full));
        editor.document.insert(Annotation::new(
            Tool::Rectangle,
            Point { x: 0., y: 0. },
            Style::default(),
        ));
        editor.document.undo();
        assert!(editor.document.can_redo());
        let eligible = shared_snapshot_allowed(&editor, &image, &image, (2, 2));
        assert!(eligible);
        let snapshot = snapshot_pixels(&image, (2, 2), eligible, || {
            panic!("无标注共享保存不得生成整图 output")
        })
        .unwrap();
        let SnapshotPixels::Shared { image: saved, .. } = &snapshot else {
            panic!("已处理底图没有共享");
        };
        assert!(Arc::ptr_eq(saved, &image));
        let (size, bgra) = snapshot.pixels().unwrap();
        assert_eq!(size, (2, 2));
        assert_eq!(bgra, expected);
        assert_eq!(bgra.as_ptr(), image.as_bytes(0).unwrap().as_ptr());
        assert_eq!(source.bgra[0..4], [8, 32, 96, 128]);
    }

    #[test]
    fn pin_workspace_shared_snapshot_preserves_annotation_fallback_and_source_guards() {
        let full = Rect::full(16, 16);
        let raw = [20, 30, 40, 255].repeat(16 * 16);
        let image = crate::pin_view::preview(Image {
            width: 16,
            height: 16,
            bgra: raw.clone(),
            readback_ms: 0.,
        })
        .unwrap();
        let mut editor = Editor::new(full, Some(full));
        let mut rectangle = Annotation::new(
            Tool::Rectangle,
            Point { x: 4., y: 4. },
            Style {
                color: 0xff0000,
                filled: true,
                radius: 0.,
                opacity: 1.,
                ..Default::default()
            },
        );
        rectangle.update(Point { x: 12., y: 12. });
        editor.document.insert(rectangle.clone());
        let eligible = shared_snapshot_allowed(&editor, &image, &image, (16, 16));
        assert!(!eligible);
        let calls = std::cell::Cell::new(0);
        let snapshot = snapshot_pixels(&image, (16, 16), eligible, || {
            calls.set(calls.get() + 1);
            let source = SourcePixels {
                fill: None,
                canvas: full,
                planes: vec![PixelPlane {
                    rect: full,
                    bgra: image.as_bytes(0).unwrap(),
                }],
                overlay: None,
            };
            let scene = Scene::prepare(&editor.document_annotations(), &source);
            assert!(scene.errors.is_empty());
            let mut output = source.read(full);
            scene.apply(&mut output, full);
            Ok(output)
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(matches!(snapshot, SnapshotPixels::Output(_)));
        let (_, bgra) = snapshot.pixels().unwrap();
        let center = (8 * 16 + 8) * 4;
        assert_eq!(&bgra[center..center + 4], &[0, 0, 255, 255]);
        assert_eq!(&bgra[..4], &[20, 30, 40, 255]);
        assert_eq!(image.as_bytes(0).unwrap(), raw);
        assert_eq!(editor.document.items()[0].annotation, rectangle);
        let failed = snapshot_pixels(&image, (16, 16), false, || Err("合成失败".into()));
        assert!(failed.is_err());
        assert_eq!(image.as_bytes(0).unwrap(), raw);

        editor.document.clear();
        assert!(shared_snapshot_allowed(&editor, &image, &image, (16, 16)));
        let stale = crate::pin_view::preview(Image {
            width: 16,
            height: 16,
            bgra: raw,
            readback_ms: 0.,
        })
        .unwrap();
        assert!(!shared_snapshot_allowed(&editor, &stale, &image, (16, 16)));
        editor.crop = Some(Rect { width: 8, ..full });
        assert!(!shared_snapshot_allowed(&editor, &image, &image, (16, 16)));
        editor.crop = Some(full);
        editor.text_draft = Some(crate::editor::TextDraft {
            id: None,
            annotation: Annotation::new(Tool::Text, Point { x: 2., y: 2. }, Style::default()),
        });
        assert!(!shared_snapshot_allowed(&editor, &image, &image, (16, 16)));
        editor.text_draft = None;
        editor.document.crop_mask = crate::crop_mask::Polygon::new(&[
            Point { x: 2., y: 2. },
            Point { x: 12., y: 2. },
            Point { x: 12., y: 12. },
            Point { x: 2., y: 12. },
        ]);
        assert!(editor.document.crop_mask.is_some());
        assert!(!shared_snapshot_allowed(&editor, &image, &image, (16, 16)));
    }
}

#[cfg(test)]
mod window_policy_tests {
    use super::*;

    #[test]
    fn workspace_policy_only_change_triggers_clean_state_save_with_old_snapshot_stamp() {
        let exact = StateStamp {
            state: PinState::default(),
            placement: Placement {
                x: -5_000,
                y: -300,
                width: 16_384,
                height: 899,
            },
            window_sizing: PinWindowSizing::ExactCapture,
            click_through: false,
            presentation_style: None,
        };
        let general = StateStamp {
            window_sizing: PinWindowSizing::General,
            ..exact.clone()
        };
        assert!(!state_save_due(
            SnapshotDecision::Clean,
            Some(&exact),
            &exact
        ));
        assert!(state_save_due(
            SnapshotDecision::Clean,
            Some(&exact),
            &general
        ));
        assert!(state_save_due(
            SnapshotDecision::Clean,
            Some(&general),
            &exact
        ));
        assert!(!state_save_due(
            SnapshotDecision::Deferred,
            Some(&exact),
            &general
        ));
        // 编码回执仍记提交时的 exact，下一tick不得把当前general当成已保存。
        let acknowledged = exact.clone();
        assert_eq!(acknowledged.window_sizing, PinWindowSizing::ExactCapture);
        assert!(state_save_due(
            SnapshotDecision::Clean,
            Some(&acknowledged),
            &general
        ));
    }
}
