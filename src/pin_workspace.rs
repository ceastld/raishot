//! 贴图工作区：纯分组模型、有界本机清单和窗口租约。不持有 GPUI 或 GPU。
use crate::{
    capture::Image,
    export,
    pin_model::{Adjustments, PinState, PinWindowSizing, Placement, WireProcessingHistory},
    pin_raster_document::WireRasterDocument,
    pin_text_document::WireTextDocument,
    selection_finish::SelectionFinishStyle,
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::Read,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[path = "pin_workspace_missing.rs"]
mod missing;
pub(crate) use missing::prune_missing_records;
#[path = "pin_workspace_db.rs"]
mod db;
#[path = "pin_workspace_relocation.rs"]
mod relocation;

pub(crate) const DEFAULT_GROUP_ID: &str = "0";
const MAX_GROUPS: usize = 32;
const MAX_ACTIVE: usize = 50;
const MAX_CLOSED: usize = 20;
const MAX_MANIFEST_BYTES: u64 = 512 * 1024;
const MAX_DOCUMENT_BYTES: usize = 512 * 1024;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const CLOSED_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Group {
    pub id: String,
    pub name: String,
}
impl Group {
    /// 默认分组名以中文持久化，显示时按界面语言本地化；用户改名后原样显示。
    pub(crate) fn display_name(&self) -> String {
        if self.id == DEFAULT_GROUP_ID && self.name == "默认" {
            crate::tr!("默认", "Default").to_owned()
        } else {
            self.name.clone()
        }
    }
}
/// 新贴图默认标题以中文持久化；显示时本地化默认值与空标题。
pub(crate) fn display_pin_title(title: &str) -> String {
    match title {
        "" => crate::tr!("未命名贴图", "Untitled pin").to_owned(),
        "贴图" => crate::tr!("贴图", "Pin").to_owned(),
        _ => title.to_owned(),
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinStatus {
    Active,
    Closed,
}
#[derive(Clone, Debug)]
pub(crate) struct PinEntry {
    pub id: String,
    pub group_id: String,
    pub title: String,
    pub status: PinStatus,
    pub hidden: bool,
    pub click_through: bool,
    pub placement: Placement,
    /// 窗口政策独立于保存尺寸；旧清单缺字段沿 General，不猜截图来源。
    pub window_sizing: PinWindowSizing,
    pub state: PinState,
    pub image_file: Option<String>,
    pub text_document: Option<WireTextDocument>,
    /// 无外观；有 raster_document 时仍按原图恢复矢量标注，旧烘焙快照不添加外观。
    pub presentation_style: Option<SelectionFinishStyle>,
    pub original_file: Option<String>,
    pub raster_document: Option<WireRasterDocument>,
    pub processing_history: Option<WireProcessingHistory>,
    pub updated_ms: u64,
}
#[derive(Clone, Debug)]
pub(crate) struct Workspace {
    pub groups: Vec<Group>,
    pub current_group: String,
    pub restore_on_startup: bool,
    pub pins: Vec<PinEntry>,
    pub revision: u64,
}
/// 管理列表不加载文档及撤销两栈；恢复操作仍使用完整 Workspace/PinEntry。
#[derive(Clone, Debug)]
pub(crate) struct WorkspaceSummary {
    pub groups: Vec<Group>,
    pub current_group: String,
    pub restore_on_startup: bool,
    pub revision: u64,
    pub pins: Vec<PinSummary>,
}
#[derive(Clone, Debug)]
pub(crate) struct PinSummary {
    pub id: String,
    pub group_id: String,
    pub title: String,
    pub status: PinStatus,
    pub hidden: bool,
    pub click_through: bool,
    pub placement: Placement,
    pub window_sizing: PinWindowSizing,
    pub state: PinState,
    pub image_file: Option<String>,
    pub original_file: Option<String>,
    pub presentation_style: Option<SelectionFinishStyle>,
    pub updated_ms: u64,
    pub text: bool,
    pub preview_text: Option<String>,
    pub has_raster_document: bool,
}
impl Default for Workspace {
    fn default() -> Self {
        Self {
            groups: vec![Group {
                id: DEFAULT_GROUP_ID.into(),
                name: "默认".into(),
            }],
            current_group: DEFAULT_GROUP_ID.into(),
            restore_on_startup: true,
            pins: Vec::new(),
            revision: 0,
        }
    }
}
pub(crate) enum Action {
    Create { name: String },
    Rename { id: String, name: String },
    Reorder { ids: Vec<String> },
    DeleteEmptyGroup { id: String },
    SwitchGroup { id: String },
    MovePin { id: String, group_id: String },
    SetTitle { id: String, title: String },
    SetHidden { id: String, hidden: bool },
    SetRestore { enabled: bool },
    Close { id: String },
    Activate { id: String },
}

pub(crate) fn load() -> Result<Workspace> {
    load_unlocked_in(&folder())
}
pub(crate) fn load_summary() -> Result<WorkspaceSummary> {
    let dir = folder();
    let summary = db::load_summary(&dir)?;
    relocation::reconcile(&dir)?;
    Ok(summary)
}
pub(crate) fn reconcile_legacy_assets() -> Result<()> {
    relocation::reconcile(&folder())
}
/// 启动完成后的后台准备；纯空配置不会因此创建数据库。
pub(crate) fn prepare_existing_workspace() -> Result<usize> {
    relocation::prepare(&folder())
}
/// 启动检查只读已有来源，不建库、建表或触发旧数据迁移。
pub(crate) fn startup_restore_needed() -> Result<bool> {
    db::startup_restore_needed(&folder())
}
pub(crate) fn apply(action: Action) -> Result<Workspace> {
    apply_in(&folder(), action)
}
pub(crate) fn asset_path(image_file: &str) -> Result<PathBuf> {
    asset_path_in(&folder(), image_file)
}
pub(crate) fn scope() -> u64 {
    let root = crate::preferences::data_dir();
    let root = std::path::absolute(&root).unwrap_or(root);
    let text = root
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase();
    let text = text.strip_prefix("\\\\?\\").unwrap_or(&text);
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in text.as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
    hash.max(1)
}

pub(crate) struct Session {
    pub id: String,
    pub owner_generation: u64,
    lease: Option<File>,
    lease_id: String,
    dir: PathBuf,
    rollback_empty: bool,
    pub(crate) capture_pixels:
        Option<std::sync::Arc<crate::pin_raster_document::capture_source::CapturePixels>>,
}
impl Session {
    pub(crate) fn register(state: PinState, placement: Placement) -> Result<Self> {
        Self::register_in(&folder(), state, placement)
    }
    /// 返回同一成功登记事务中的记录，调用者无需再读清单查找自己的新条目。
    pub(crate) fn register_with_entry(
        state: PinState,
        placement: Placement,
        window_sizing: PinWindowSizing,
    ) -> Result<(Self, PinEntry)> {
        Self::register_with_sizing_in(&folder(), state, placement, window_sizing)
    }
    fn register_in(dir: &Path, state: PinState, placement: Placement) -> Result<Self> {
        Self::register_with_entry_in(dir, state, placement).map(|(session, _)| session)
    }
    fn register_with_entry_in(
        dir: &Path,
        state: PinState,
        placement: Placement,
    ) -> Result<(Self, PinEntry)> {
        Self::register_with_sizing_in(dir, state, placement, PinWindowSizing::General)
    }
    fn register_with_sizing_in(
        dir: &Path,
        state: PinState,
        placement: Placement,
        window_sizing: PinWindowSizing,
    ) -> Result<(Self, PinEntry)> {
        validate_state(&state, placement)?;
        validate_window_sizing(window_sizing, placement)?;
        let _lock = global_lock(dir)?;
        let mut workspace = load_in(dir)?;
        let id = new_id("pin");
        let lease = new_lease(dir, &id)?;
        let owner_generation = match next_control_revision(&workspace) {
            Ok(generation) => generation,
            Err(error) => {
                drop(lease);
                let _ = std::fs::remove_file(lease_path(dir, &id)?);
                return Err(error);
            }
        };
        let mut session = Self {
            id: id.clone(),
            owner_generation,
            lease: Some(lease),
            lease_id: id.clone(),
            dir: dir.to_path_buf(),
            rollback_empty: true,
            capture_pixels: None,
        };
        workspace.pins.push(PinEntry {
            id: id.clone(),
            group_id: workspace.current_group.clone(),
            title: "贴图".into(),
            status: PinStatus::Active,
            hidden: false,
            click_through: false,
            placement,
            window_sizing,
            state,
            image_file: None,
            text_document: None,
            presentation_style: None,
            original_file: None,
            raster_document: None,
            processing_history: None,
            updated_ms: now_ms(),
        });
        if let Err(error) = commit(dir, &mut workspace, Some(&id), None) {
            session.rollback_empty = false;
            drop(session.lease.take());
            let _ = std::fs::remove_file(lease_path(dir, &id)?);
            return Err(error);
        }
        let entry = workspace
            .pins
            .into_iter()
            .find(|pin| pin.id == id)
            .ok_or("新贴图工作区记录缺失")?;
        Ok((session, entry))
    }
    /// 取得租约后锁内登记新代次再解码；不改记录状态，失败仅释放租约。
    pub(crate) fn claim(id: &str) -> Result<(Self, Image, PinEntry)> {
        Self::claim_in(&folder(), id)
    }
    fn claim_in(dir: &Path, id: &str) -> Result<(Self, Image, PinEntry)> {
        validate_pin_id(id)?;
        let lock = global_lock(dir)?;
        let mut workspace = load_in(dir)?;
        let entry = workspace
            .pins
            .iter()
            .find(|pin| pin.id == id)
            .cloned()
            .ok_or(tr!("贴图记录不存在", "The pin record doesn't exist"))?;
        let canonical = entry.image_file.as_deref().ok_or("贴图尚未完成首次快照")?;
        let filename = if entry.raster_document.is_some() {
            entry
                .original_file
                .as_deref()
                .ok_or("截图贴图缺少处理原图")?
        } else {
            canonical
        };
        let path = asset_path_in(dir, filename)?;
        let lease = acquire_lease(dir, id)?;
        let owner_generation = commit_control_revision(dir, &mut workspace)?;
        let mut session = Self {
            id: id.into(),
            owner_generation,
            lease: Some(lease),
            lease_id: id.into(),
            dir: dir.to_path_buf(),
            rollback_empty: false,
            capture_pixels: None,
        };
        // 租约保护记录与资源；大图解码不占全局事务锁。
        drop(lock);
        for name in [Some(canonical), entry.original_file.as_deref()]
            .into_iter()
            .flatten()
        {
            ensure_regular_asset(dir, name)?;
        }
        let image = export::read_image(&path)?;
        if let Some(document) = &entry.raster_document {
            if entry
                .state
                .adjustments
                .dimensions(image.width, image.height)
                != document.storage_size()
            {
                return Err("截图贴图原图与处理参数、文档尺寸不匹配".into());
            }
        }
        if let Some(history) = &entry.processing_history {
            let document = entry
                .raster_document
                .as_ref()
                .ok_or("处理历史缺少图片文档")?;
            if png_dimensions(&asset_path_in(dir, canonical)?)? != document.size() {
                return Err("处理历史的实际处理后 PNG 与文档尺寸不匹配".into());
            }
            history.validate(
                entry.state.adjustments,
                (image.width, image.height),
                document.size(),
            )?;
        }
        if let Some(source) = entry
            .raster_document
            .as_ref()
            .and_then(|document| document.capture_source())
        {
            if entry.state.adjustments != Adjustments::default()
                || entry.processing_history.is_some()
            {
                return Err("截图完整来源与旧处理历史不兼容".into());
            }
            session.capture_pixels = Some(
                crate::pin_raster_document::capture_source::resource::load_context(
                    &path, source, &image,
                )?,
            );
        }
        Ok((session, image, entry))
    }
    /// 只有持有原记录独占租约的活动窗口可以保存自己的交互状态。
    pub(crate) fn persist_interaction(&self, locked: bool, click_through: bool) -> Result<()> {
        if self.lease.is_none() || self.id != self.lease_id {
            return Err("贴图交互状态保存缺少自身租约".into());
        }
        validate_pin_id(&self.id)?;
        let _lock = global_lock(&self.dir)?;
        db::persist_interaction(&self.dir, &self.id, locked, click_through)
    }
    pub(crate) fn mark_active(&self) -> Result<()> {
        apply_in(
            &self.dir,
            Action::Activate {
                id: self.id.clone(),
            },
        )
        .map(|_| ())
    }
    /// 正式关闭且用户关闭恢复记录时，允许持有租约的窗口删除自己的记录。
    pub(crate) fn forget(&self) -> Result<()> {
        forget_in(&self.dir, &self.id, true)
    }
}

/// 通过原事务锁分配唯一控制编号，调用方在锁内核对并写入原生请求。
pub(crate) fn with_control_ticket<T>(id: &str, f: impl FnOnce(u64) -> Result<T>) -> Result<T> {
    with_control_ticket_in(&folder(), id, f)
}
fn with_control_ticket_in<T>(dir: &Path, id: &str, f: impl FnOnce(u64) -> Result<T>) -> Result<T> {
    validate_pin_id(id)?;
    let _lock = global_lock(dir)?;
    let mut workspace = load_in(dir)?;
    if !workspace
        .pins
        .iter()
        .any(|pin| pin.id == id && pin.status == PinStatus::Active)
        || !lease_is_live(dir, id)
    {
        return Err("贴图没有持有租约的活动窗口，控制请求未发送".into());
    }
    let ticket = commit_control_revision(dir, &mut workspace)?;
    // 即便原生发送失败，已持久化的编号也不能被下一次请求复用。
    f(ticket)
}
pub(crate) fn with_control_lock<T>(f: impl FnOnce() -> Result<T>) -> Result<T> {
    with_control_lock_in(&folder(), f)
}
fn with_control_lock_in<T>(dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _lock = global_lock(dir)?;
    f()
}
pub(crate) fn lease_live(id: &str) -> bool {
    validate_pin_id(id).is_ok() && lease_is_live(&folder(), id)
}

fn next_control_revision(workspace: &Workspace) -> Result<u64> {
    let revision = workspace
        .revision
        .checked_add(1)
        .ok_or("工作区控制编号已用尽")?;
    usize::try_from(revision).map_err(|_| "工作区控制编号超过当前平台范围")?;
    Ok(revision)
}
fn commit_control_revision(dir: &Path, workspace: &mut Workspace) -> Result<u64> {
    let revision = next_control_revision(workspace)?;
    workspace.revision = revision;
    write_manifest(dir, &encoded_manifest(workspace)?)?;
    Ok(revision)
}
impl Drop for Session {
    fn drop(&mut self) {
        drop(self.lease.take());
        if self.rollback_empty {
            // 新窗口未得到首次图片时无可恢复内容；失败清理也不等待其它进程。
            if let Ok(_lock) = global_lock(&self.dir) {
                if let Ok(mut workspace) = load_in(&self.dir) {
                    if workspace
                        .pins
                        .iter()
                        .any(|pin| pin.id == self.id && pin.image_file.is_none())
                    {
                        workspace.pins.retain(|pin| pin.id != self.id);
                        let _ = commit(&self.dir, &mut workspace, None, None);
                    }
                }
            }
        }
    }
}

pub(crate) fn save_snapshot(
    id: &str,
    image: &Image,
    state: &PinState,
    p: Placement,
    status: PinStatus,
) -> Result<()> {
    save_snapshot_in(&folder(), id, image, state, p, status)
}
/// 与 canonical PNG 同次提交文字来源；缺省来源会清除上一版的文字元数据。
/// 编码闭包消费其持有的像素源；返回后先释放 capture，再进行资源和清单提交。
/// 调用方须编码完整的指定尺寸 PNG，沿用 export 的像素验证与原子写入。
pub(crate) fn save_snapshot_with_text_document(
    id: &str,
    size: (u32, u32),
    state: &PinState,
    window: (Placement, PinWindowSizing),
    status: PinStatus,
    text_document: Option<WireTextDocument>,
    encode: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    save_snapshot_with_policy_in(
        &folder(),
        id,
        size,
        (state, window.0, Some(window.1)),
        status,
        text_document,
        encode,
    )
}
fn save_snapshot_in(
    dir: &Path,
    id: &str,
    image: &Image,
    state: &PinState,
    p: Placement,
    status: PinStatus,
) -> Result<()> {
    save_snapshot_pixels_in(
        dir,
        id,
        (image.width, image.height),
        &image.bgra,
        state,
        p,
        status,
    )
}
fn save_snapshot_pixels_in(
    dir: &Path,
    id: &str,
    size: (u32, u32),
    bgra: &[u8],
    state: &PinState,
    p: Placement,
    status: PinStatus,
) -> Result<()> {
    save_snapshot_with_encoder_in(dir, id, size, state, p, status, |path| {
        export::save_png_pixels(size.0, size.1, bgra, path)
    })
}
fn save_snapshot_with_encoder_in(
    dir: &Path,
    id: &str,
    size: (u32, u32),
    state: &PinState,
    p: Placement,
    status: PinStatus,
    encode: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    save_snapshot_with_text_document_in(dir, id, size, state, p, status, None, encode)
}
#[allow(clippy::too_many_arguments)]
fn save_snapshot_with_text_document_in(
    dir: &Path,
    id: &str,
    size: (u32, u32),
    state: &PinState,
    p: Placement,
    status: PinStatus,
    text_document: Option<WireTextDocument>,
    encode: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    save_snapshot_with_policy_in(
        dir,
        id,
        size,
        (state, p, None),
        status,
        text_document,
        encode,
    )
}
fn save_snapshot_with_policy_in(
    dir: &Path,
    id: &str,
    size: (u32, u32),
    window: (&PinState, Placement, Option<PinWindowSizing>),
    status: PinStatus,
    text_document: Option<WireTextDocument>,
    encode: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let (state, p, window_sizing) = window;
    validate_pin_id(id)?;
    validate_state(state, p)?;
    if let Some(sizing) = window_sizing {
        validate_window_sizing(sizing, p)?;
    }
    if size.0 > 65535 || size.1 > 65535 || u64::from(size.0) * u64::from(size.1) > 32_000_000 {
        return Err("贴图快照超过 65535 单边或 3200 万像素限制".into());
    }
    if let Some(doc) = &text_document {
        doc.validate()?;
        if (doc.width, doc.height) != size || state.adjustments != Adjustments::default() {
            return Err("纯文字来源与贴图快照尺寸或图像处理状态不匹配".into());
        }
    }
    // 编码和写盘仍在全局锁外；复用流式 PNG，只额外保留一行通道转换缓冲。
    let (mut pending, encoded_len) = PendingAsset::encode_in(dir, id, encode)?;
    let _lock = global_lock(dir)?;
    let mut workspace = load_in(dir)?;
    // 文件名在锁内确定，同一贴图的新旧快照并存到清单提交为止。
    let stem = pin_asset_stem(dir, &workspace, id)?;
    let filename = next_asset_name(dir, &stem, false)?;
    pending.path = Some(asset_path_in(dir, &filename)?);
    let pin = workspace
        .pins
        .iter_mut()
        .find(|pin| pin.id == id)
        .ok_or(tr!("贴图记录不存在", "The pin record doesn't exist"))?;
    pin.image_file = Some(filename.clone());
    pin.text_document = text_document;
    pin.presentation_style = None;
    pin.original_file = None;
    pin.raster_document = None;
    pin.processing_history = None;
    pin.state = state.clone();
    pin.placement = p;
    if let Some(sizing) = window_sizing {
        pin.window_sizing = sizing;
    }
    pin.status = status;
    pin.updated_ms = now_ms();
    if status == PinStatus::Closed {
        move_to_end(&mut workspace, id);
    }
    // 数量/期限政策先执行，再按完整清单字节裁当前记录的远端文字历史。
    // 当前内容与其它记录历史不裁；21 条闭合记录不能先触发验证拒绝。
    prune_record_limits(dir, &mut workspace, Some(id))?;
    encoded_text_manifest(&mut workspace, id)?;
    // 先在内存完成容量校验，刚保存的记录永不作为修剪候选。
    prune(
        dir,
        &mut workspace,
        Some(id),
        Some((&filename, encoded_len)),
    )?;
    bump_revision(&mut workspace)?;
    let text = encoded_text_manifest(&mut workspace, id)?;
    // 唯一资源路径使用 no-replace；现有资源即使同名也不能被覆盖。
    move_new(
        &pending.temp,
        pending.path.as_deref().ok_or("贴图资源路径缺失")?,
    )?;
    pending.renamed = true;
    write_manifest(dir, &text)?;
    pending.committed = true;
    cleanup_orphans(dir, &workspace, Some(id));
    Ok(())
}
#[derive(Clone)]
pub(crate) struct RasterSnapshotMetadata {
    pub original_size: (u32, u32),
    pub presentation_style: Option<SelectionFinishStyle>,
    pub document: WireRasterDocument,
    pub processing_history: Option<WireProcessingHistory>,
}
pub(crate) fn validate_presentation_style(style: SelectionFinishStyle) -> Result<()> {
    if style != style.normalized() {
        return Err("截图贴图外观必须是规范化参数".into());
    }
    Ok(())
}
/// 两份 PNG 与外观、文档、处理参数在同一清单事务中发布；任一失败保留上一版。
pub(crate) fn save_raster_snapshot_with_encoders(
    id: &str,
    state: &PinState,
    window: (Placement, PinWindowSizing),
    status: PinStatus,
    metadata: RasterSnapshotMetadata,
    encode_current: impl FnOnce(&Path) -> Result<()>,
    encode_original: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    save_raster_snapshot_with_policy_in(
        &folder(),
        id,
        (state, window.0, Some(window.1)),
        status,
        metadata,
        encode_current,
        encode_original,
    )
}
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn save_raster_snapshot_with_encoders_in(
    dir: &Path,
    id: &str,
    state: &PinState,
    p: Placement,
    status: PinStatus,
    metadata: RasterSnapshotMetadata,
    encode_current: impl FnOnce(&Path) -> Result<()>,
    encode_original: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    save_raster_snapshot_with_policy_in(
        dir,
        id,
        (state, p, None),
        status,
        metadata,
        encode_current,
        encode_original,
    )
}
fn save_raster_snapshot_with_policy_in(
    dir: &Path,
    id: &str,
    window: (&PinState, Placement, Option<PinWindowSizing>),
    status: PinStatus,
    metadata: RasterSnapshotMetadata,
    encode_current: impl FnOnce(&Path) -> Result<()>,
    encode_original: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let (state, p, window_sizing) = window;
    validate_pin_id(id)?;
    validate_state(state, p)?;
    if let Some(sizing) = window_sizing {
        validate_window_sizing(sizing, p)?;
    }
    if let Some(style) = metadata.presentation_style {
        validate_presentation_style(style)?;
    }
    metadata.document.validate()?;
    let original = metadata.original_size;
    if original.0 == 0
        || original.1 == 0
        || original.0 > 65535
        || original.1 > 65535
        || u64::from(original.0) * u64::from(original.1) > 32_000_000
        || state.adjustments.dimensions(original.0, original.1) != metadata.document.storage_size()
    {
        return Err("截图贴图原图与处理参数、文档尺寸不匹配".into());
    }
    if let Some(history) = &metadata.processing_history {
        history.validate(state.adjustments, original, metadata.document.size())?;
    }
    if metadata.document.capture_source().is_some()
        && (state.adjustments != Adjustments::default() || metadata.processing_history.is_some())
    {
        return Err("截图完整来源不能发布旧处理参数或截断两栈".into());
    }
    // 编码闭包在全局锁外消费自身像素；两份资产尚未被清单引用。
    let (mut raw, raw_len) = PendingAsset::encode_in(dir, id, encode_original)?;
    let (mut current, current_len) = PendingAsset::encode_in(dir, id, encode_current)?;
    if let Some(source) = metadata.document.capture_source() {
        let raw_image = export::read_image(&raw.temp)?;
        // 自有PNG仍在事务暂存区；坏来源不能成为新的恢复依据。
        crate::pin_raster_document::capture_source::resource::load_context(
            &raw.temp, source, &raw_image,
        )?;
        if png_dimensions(&current.temp)? != metadata.document.storage_size() {
            return Err("截图可见兼容帧与固定内容范围不匹配".into());
        }
    }
    if metadata.processing_history.is_some()
        && (png_dimensions(&raw.temp)? != original
            || png_dimensions(&current.temp)? != metadata.document.size())
    {
        return Err("处理历史的原图或处理后 PNG 与真实快照尺寸不匹配".into());
    }
    let _lock = global_lock(dir)?;
    let mut workspace = load_in(dir)?;
    let stem = pin_asset_stem(dir, &workspace, id)?;
    let current_name = next_asset_name(dir, &stem, false)?;
    let raw_name = next_asset_name(dir, &stem, true)?;
    current.path = Some(asset_path_in(dir, &current_name)?);
    raw.path = Some(asset_path_in(dir, &raw_name)?);
    let pin = workspace
        .pins
        .iter_mut()
        .find(|pin| pin.id == id)
        .ok_or("截图贴图记录不存在")?;
    pin.image_file = Some(current_name.clone());
    pin.original_file = Some(raw_name.clone());
    pin.presentation_style = metadata.presentation_style;
    pin.raster_document = Some(metadata.document);
    pin.processing_history = metadata.processing_history;
    pin.text_document = None;
    pin.state = state.clone();
    pin.placement = p;
    if let Some(sizing) = window_sizing {
        pin.window_sizing = sizing;
    }
    pin.status = status;
    pin.updated_ms = now_ms();
    if status == PinStatus::Closed {
        move_to_end(&mut workspace, id);
    }
    // 先沿既有保留政策修剪过期／超量记录，允许第21张关闭记录正常落盘。
    prune_record_limits(dir, &mut workspace, Some(id))?;
    // 数量已合法后才核清单字节；仅裁当前文档的最远历史。
    encoded_raster_manifest(&mut workspace, id)?;
    prune_with_assets(
        dir,
        &mut workspace,
        Some(id),
        &[(&current_name, current_len), (&raw_name, raw_len)],
        MAX_BYTES,
    )?;
    bump_revision(&mut workspace)?;
    let manifest = encoded_raster_manifest(&mut workspace, id)?;
    move_new(
        &current.temp,
        current.path.as_deref().ok_or("截图贴图资产路径缺失")?,
    )?;
    current.renamed = true;
    move_new(
        &raw.temp,
        raw.path.as_deref().ok_or("截图贴图原图路径缺失")?,
    )?;
    raw.renamed = true;
    write_manifest(dir, &manifest)?;
    current.committed = true;
    raw.committed = true;
    cleanup_orphans(dir, &workspace, Some(id));
    Ok(())
}
/// 只读取已编码 PNG 的头部尺寸，不另分配整图，也不借窗口外框证明内容尺寸。
fn png_dimensions(path: &Path) -> Result<(u32, u32)> {
    Ok(image::ImageReader::with_format(
        std::io::BufReader::new(File::open(path)?),
        image::ImageFormat::Png,
    )
    .into_dimensions()?)
}
/// 编码暂存在 .runtime/，以贴图标识开头，便于按租约判断能否回收。
fn asset_temp_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_pin_id(id)?;
    Ok(ensure_runtime_dir(dir)?.join(format!("{id}-asset-{}.png.tmp", unique_suffix())))
}
struct PendingAsset {
    temp: PathBuf,
    /// 最终路径在全局锁内确定。
    path: Option<PathBuf>,
    renamed: bool,
    committed: bool,
}
impl PendingAsset {
    fn encode_in(
        dir: &Path,
        id: &str,
        encode: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<(Self, u64)> {
        std::fs::create_dir_all(dir)?;
        let temp = asset_temp_path(dir, id)?;
        drop(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?,
        );
        let pending = Self {
            temp,
            path: None,
            renamed: false,
            committed: false,
        };
        // 先占有唯一临时路径，export 的原子写入仅替换本次创建的占位文件。
        // PNG 内部也有原生重命名；沿用工作区的 verbatim 路径以保留深层目录支持。
        // FnOnce 调用消费闭包；成功或失败都会在继续提交之前释放其捕获的像素源。
        encode(&std::fs::canonicalize(&pending.temp)?)?;
        let file = OpenOptions::new().write(true).open(&pending.temp)?;
        file.sync_all()?;
        let bytes = file.metadata()?.len();
        if bytes >= MAX_BYTES {
            return Err(tr!(
                "这张贴图太大，超出贴图记录的容量上限",
                "This pin is too large for the pin record storage limit"
            )
            .into());
        }
        Ok((pending, bytes))
    }
}
impl Drop for PendingAsset {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.temp);
            if let Some(path) = self.path.as_deref().filter(|_| self.renamed) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}
pub(crate) fn update_state(id: &str, state: &PinState, p: Placement) -> Result<()> {
    update_state_in(&folder(), id, state, p)
}
pub(crate) fn update_state_with_sizing(
    id: &str,
    state: &PinState,
    p: Placement,
    window_sizing: PinWindowSizing,
) -> Result<()> {
    update_state_with_sizing_in(&folder(), id, state, p, Some(window_sizing))
}
fn update_state_in(dir: &Path, id: &str, state: &PinState, p: Placement) -> Result<()> {
    update_state_with_sizing_in(dir, id, state, p, None)
}
fn update_state_with_sizing_in(
    dir: &Path,
    id: &str,
    state: &PinState,
    p: Placement,
    window_sizing: Option<PinWindowSizing>,
) -> Result<()> {
    if let Some(sizing) = window_sizing {
        validate_window_sizing(sizing, p)?;
    }
    validate_pin_id(id)?;
    validate_state(state, p)?;
    let _lock = global_lock(dir)?;
    if db::update_state(dir, id, state, p, window_sizing)? {
        return Ok(());
    }
    let mut workspace = load_in(dir)?;
    let pin = workspace
        .pins
        .iter_mut()
        .find(|pin| pin.id == id)
        .ok_or(tr!("贴图记录不存在", "The pin record doesn't exist"))?;
    if pin.placement == p
        && WireState::from(&pin.state) == WireState::from(state)
        && window_sizing.is_none_or(|sizing| pin.window_sizing == sizing)
    {
        return Ok(());
    }
    pin.state = state.clone();
    pin.placement = p;
    if let Some(sizing) = window_sizing {
        pin.window_sizing = sizing;
    }
    pin.updated_ms = now_ms();
    commit(dir, &mut workspace, Some(id), None)
}
pub(crate) fn closed_current() -> Result<Vec<PinEntry>> {
    closed_in(&folder())
}
fn closed_in(dir: &Path) -> Result<Vec<PinEntry>> {
    let workspace = load_unlocked_in(dir)?;
    // 关闭时移到清单末尾；标题、隐藏等后续更新不改变最近关闭的顺序。
    Ok(workspace
        .pins
        .into_iter()
        .rev()
        .filter(|pin| {
            pin.status == PinStatus::Closed
                && pin.group_id == workspace.current_group
                && pin.image_file.is_some()
                && !lease_is_live(dir, &pin.id)
        })
        .collect())
}
pub(crate) fn restorable_active() -> Result<Vec<PinEntry>> {
    restorable_in(&folder())
}
/// 手动恢复不受启动偏好或 Active/Closed 状态限制；租约仍由 claim 原子取得。
pub(crate) fn manual_restore_ids(workspace: &Workspace) -> HashSet<String> {
    manual_restore_ids_in(&folder(), workspace)
}
pub(crate) fn manual_restore_summary_ids(workspace: &WorkspaceSummary) -> HashSet<String> {
    let dir = folder();
    workspace
        .pins
        .iter()
        .filter(|pin| {
            !lease_is_live(&dir, &pin.id)
                && restore_assets_available(
                    &dir,
                    pin.image_file.as_deref(),
                    pin.original_file.as_deref(),
                )
        })
        .map(|pin| pin.id.clone())
        .collect()
}
fn manual_restore_ids_in(dir: &Path, workspace: &Workspace) -> HashSet<String> {
    workspace
        .pins
        .iter()
        .filter(|pin| {
            !lease_is_live(dir, &pin.id)
                && restore_assets_available(
                    dir,
                    pin.image_file.as_deref(),
                    pin.original_file.as_deref(),
                )
        })
        .map(|pin| pin.id.clone())
        .collect()
}
fn restore_assets_available(dir: &Path, image: Option<&str>, original: Option<&str>) -> bool {
    image.is_some_and(|name| ensure_regular_asset(dir, name).is_ok())
        && original.is_none_or(|name| ensure_regular_asset(dir, name).is_ok())
}
/// 符号链接的 symlink_metadata 不算普通文件，工作区外的链接目标不会被读取。
fn ensure_regular_asset(dir: &Path, filename: &str) -> Result<()> {
    if !std::fs::symlink_metadata(asset_path_in(dir, filename)?)?.is_file() {
        return Err("贴图资源必须是工作区内的普通文件".into());
    }
    Ok(())
}
fn restorable_in(dir: &Path) -> Result<Vec<PinEntry>> {
    let workspace = load_unlocked_in(dir)?;
    if !workspace.restore_on_startup {
        return Ok(Vec::new());
    }
    Ok(workspace
        .pins
        .into_iter()
        .filter(|pin| {
            pin.status == PinStatus::Active
                && pin.image_file.is_some()
                && !lease_is_live(dir, &pin.id)
        })
        .collect())
}
pub(crate) fn forget(id: &str) -> Result<()> {
    forget_in(&folder(), id, false)
}
fn forget_in(dir: &Path, id: &str, own_lease: bool) -> Result<()> {
    validate_pin_id(id)?;
    let _lock = global_lock(dir)?;
    if !own_lease && lease_is_live(dir, id) {
        return Err(tr!(
            "贴图仍在使用，请先关闭对应窗口",
            "The pin is still in use; close its window first"
        )
        .into());
    }
    let mut workspace = load_in(dir)?;
    let removed_assets: Vec<String> = workspace
        .pins
        .iter()
        .filter(|pin| pin.id == id)
        .flat_map(|pin| {
            [&pin.image_file, &pin.original_file]
                .into_iter()
                .flatten()
                .cloned()
        })
        .collect();
    let before = workspace.pins.len();
    workspace.pins.retain(|pin| pin.id != id);
    if workspace.pins.len() != before {
        commit(dir, &mut workspace, None, None)?;
    }
    // 只在清单删除成功后回收资源；持有租约的窗口也能删除自己的敏感图片。
    // 其它旧来源暂不可读不能阻止显式删除本项，也不因此扫描回收其它记录的资源。
    for name in removed_assets {
        if workspace.pins.iter().any(|pin| {
            [&pin.image_file, &pin.original_file]
                .into_iter()
                .flatten()
                .any(|registered| registered == &name)
        }) {
            continue;
        }
        let path = asset_path_in(dir, &name)?;
        crate::image_collection::ensure_local_file_or_missing(&path)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => std::fs::remove_file(path)?,
            Ok(_) => return Err("待删除贴图资源不是本地普通文件".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    cleanup_orphans(dir, &workspace, own_lease.then_some(id));
    Ok(())
}
/// 已关闭记录按保留政策自动清理，界面只提供逐条删除（可撤销）；这里只留给测试构造状态。
#[cfg(test)]
fn clear_closed_in(dir: &Path) -> Result<()> {
    let _lock = global_lock(dir)?;
    let mut workspace = load_in(dir)?;
    let before = workspace.pins.len();
    workspace
        .pins
        .retain(|pin| pin.status != PinStatus::Closed || lease_is_live(dir, &pin.id));
    if workspace.pins.len() != before {
        commit(dir, &mut workspace, None, None)?;
    }
    Ok(())
}

fn apply_in(dir: &Path, action: Action) -> Result<Workspace> {
    let _lock = global_lock(dir)?;
    let mut workspace = load_in(dir)?;
    let before = WireWorkspace::from(&workspace);
    let mut protected = None;
    match action {
        Action::Create { name } => {
            validate_name(&name, 64, false)?;
            if workspace.groups.len() >= MAX_GROUPS {
                return Err(tr!(
                    "最多支持 32 个贴图分组",
                    "At most 32 pin groups are supported"
                )
                .into());
            }
            let id = new_id("group");
            workspace.groups.push(Group {
                id: id.clone(),
                name: name.trim().into(),
            });
            workspace.current_group = id;
        }
        Action::Rename { id, name } => {
            validate_name(&name, 64, false)?;
            workspace
                .groups
                .iter_mut()
                .find(|group| group.id == id)
                .ok_or(tr!("分组不存在", "The group doesn't exist"))?
                .name = name.trim().into();
        }
        Action::Reorder { ids } => {
            let unique: HashSet<_> = ids.iter().collect();
            if ids.len() != workspace.groups.len()
                || unique.len() != ids.len()
                || ids
                    .iter()
                    .any(|id| !workspace.groups.iter().any(|group| &group.id == id))
            {
                return Err("分组排序必须包含每个现有分组且不能重复".into());
            }
            workspace.groups.sort_by_key(|group| {
                ids.iter()
                    .position(|id| *id == group.id)
                    .unwrap_or(usize::MAX)
            });
        }
        Action::DeleteEmptyGroup { id } => {
            if id == DEFAULT_GROUP_ID {
                return Err(tr!("默认分组不能删除", "The default group can't be deleted").into());
            }
            if !workspace.groups.iter().any(|group| group.id == id) {
                return Err(tr!("分组不存在", "The group doesn't exist").into());
            }
            if workspace.pins.iter().any(|pin| pin.group_id == id) {
                return Err(tr!(
                    "请先移走分组内的贴图（包括已关闭贴图）",
                    "Move the pins out of the group first (including closed pins)"
                )
                .into());
            }
            workspace.groups.retain(|group| group.id != id);
            if workspace.current_group == id {
                workspace.current_group = DEFAULT_GROUP_ID.into();
            }
        }
        Action::SwitchGroup { id } => {
            if !workspace.groups.iter().any(|group| group.id == id) {
                return Err(tr!("分组不存在", "The group doesn't exist").into());
            }
            workspace.current_group = id;
        }
        Action::SetRestore { enabled } => workspace.restore_on_startup = enabled,
        Action::MovePin { id, group_id } => {
            if !workspace.groups.iter().any(|group| group.id == group_id) {
                return Err(tr!("目标分组不存在", "The target group doesn't exist").into());
            }
            let pin = workspace
                .pins
                .iter_mut()
                .find(|pin| pin.id == id)
                .ok_or(tr!("贴图记录不存在", "The pin record doesn't exist"))?;
            if pin.group_id != group_id {
                pin.group_id = group_id;
                pin.updated_ms = now_ms();
            }
            protected = Some(id);
        }
        Action::SetTitle { id, title } => {
            validate_name(&title, 256, true)?;
            let pin = workspace
                .pins
                .iter_mut()
                .find(|pin| pin.id == id)
                .ok_or(tr!("贴图记录不存在", "The pin record doesn't exist"))?;
            if pin.title != title {
                pin.title = title;
                pin.updated_ms = now_ms();
            }
            protected = Some(id);
        }
        Action::SetHidden { id, hidden } => {
            let pin = workspace
                .pins
                .iter_mut()
                .find(|pin| pin.id == id)
                .ok_or(tr!("贴图记录不存在", "The pin record doesn't exist"))?;
            if pin.hidden != hidden {
                pin.hidden = hidden;
                pin.updated_ms = now_ms();
            }
            protected = Some(id);
        }
        Action::Close { id } => {
            let pin = workspace
                .pins
                .iter_mut()
                .find(|pin| pin.id == id)
                .ok_or(tr!("贴图记录不存在", "The pin record doesn't exist"))?;
            if pin.status != PinStatus::Closed {
                pin.status = PinStatus::Closed;
                pin.updated_ms = now_ms();
                move_to_end(&mut workspace, &id);
            }
            protected = Some(id);
        }
        Action::Activate { id } => {
            let pin = workspace
                .pins
                .iter_mut()
                .find(|pin| pin.id == id)
                .ok_or(tr!("贴图记录不存在", "The pin record doesn't exist"))?;
            if pin.status != PinStatus::Active {
                pin.status = PinStatus::Active;
                pin.updated_ms = now_ms();
            }
            protected = Some(id);
        }
    }
    if before != WireWorkspace::from(&workspace) {
        commit(dir, &mut workspace, protected.as_deref(), None)?;
    }
    Ok(workspace)
}
fn move_to_end(workspace: &mut Workspace, id: &str) {
    if let Some(index) = workspace.pins.iter().position(|pin| pin.id == id) {
        let pin = workspace.pins.remove(index);
        workspace.pins.push(pin);
    }
}

fn folder() -> PathBuf {
    crate::preferences::data_dir().join("pin-workspace")
}
fn manifest(dir: &Path) -> PathBuf {
    dir.join("workspace.json")
}

// 磁盘布局：
//   library.sqlite3  管理索引与独立文档表；首次访问导入 workspace.json，原文件保留作备份
//   pins/            只放图片，平铺：<创建时间>.png、<创建时间>.original.png
//   .runtime/        隐藏：事务锁、窗口租约、临时文件
// 清单里的资源名是相对 pins/ 的文件名。旧版根目录平铺布局在首次加锁时迁移。
const PINS_DIR: &str = "pins";
const RUNTIME_DIR: &str = ".runtime";
const LAYOUT_VERSION: u32 = 2;

fn runtime_dir(dir: &Path) -> PathBuf {
    dir.join(RUNTIME_DIR)
}
/// 建立运行目录并设为隐藏；隐藏属性失败只影响观感，不影响事务。
fn ensure_runtime_dir(dir: &Path) -> Result<PathBuf> {
    let runtime = runtime_dir(dir);
    if !runtime.is_dir() {
        std::fs::create_dir_all(&runtime)?;
        set_hidden(&runtime);
    }
    Ok(runtime)
}
fn set_hidden(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN},
    };
    let Ok(path) = std::path::absolute(path) else {
        return;
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let _ = unsafe { SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_HIDDEN) };
}
/// 贴图的文件名前缀 `YYYYMMDD-HHMMSS`，不含空格；同一秒的后续贴图追加 `-2`、`-3`…
fn valid_pin_stem(stem: &str) -> bool {
    let Some(base) = stem.get(..15) else {
        return false;
    };
    let shape = base.bytes().enumerate().all(|(index, byte)| match index {
        8 => byte == b'-',
        _ => byte.is_ascii_digit(),
    });
    let suffix = &stem[15..];
    shape
        && (suffix.is_empty()
            || suffix
                .strip_prefix('-')
                .is_some_and(|number| positive(number, 4, 2)))
}
/// 不带前导零、不超过 `digits` 位且不小于 `min` 的十进制数。
fn positive(number: &str, digits: usize, min: u32) -> bool {
    !number.starts_with('0')
        && number.len() <= digits
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && number.parse::<u32>().is_ok_and(|number| number >= min)
}
/// 资源文件名 `<前缀>[.original][.v<n>].png`：`.original` 是截图贴图的原图；
/// 保存时新旧快照要并存到清单提交，后写的一份带 `.v<n>`（n ≥ 2）。
/// 返回 (前缀, 是否原图, 版本号)；旧版平铺名返回 None。
fn parse_asset(name: &str) -> Option<(&str, bool, u32)> {
    let rest = name.strip_suffix(".png")?;
    let (rest, revision) = match rest.rsplit_once(".v") {
        Some((head, number)) if positive(number, 6, 2) => (head, number.parse().ok()?),
        _ => (rest, 1),
    };
    let (stem, original) = match rest.strip_suffix(".original") {
        Some(stem) => (stem, true),
        None => (rest, false),
    };
    valid_pin_stem(stem).then_some((stem, original, revision))
}
fn asset_stem(name: &str) -> Option<&str> {
    parse_asset(name).map(|(stem, _, _)| stem)
}
/// 文件名前缀按创建时间；创建时间来自贴图标识里的纳秒时间戳。
fn pin_stem_base(id: &str) -> String {
    let nanos = id
        .strip_prefix("pin-")
        .and_then(|rest| rest.split('-').next())
        .and_then(|hex| u128::from_str_radix(hex, 16).ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        });
    let seconds = i64::try_from(nanos / 1_000_000_000).unwrap_or(0) + local_offset_seconds();
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}
/// 当前本地时区相对 UTC 的偏移（按分钟取整）。
fn local_offset_seconds() -> i64 {
    use windows::Win32::{
        Foundation::SYSTEMTIME,
        System::SystemInformation::{GetLocalTime, GetSystemTime},
    };
    let seconds = |t: SYSTEMTIME| {
        days_from_civil(i64::from(t.wYear), u32::from(t.wMonth), u32::from(t.wDay)) * 86_400
            + i64::from(t.wHour) * 3600
            + i64::from(t.wMinute) * 60
            + i64::from(t.wSecond)
    };
    let (local, utc) = unsafe { (GetLocalTime(), GetSystemTime()) };
    ((seconds(local) - seconds(utc)) as f64 / 60.).round() as i64 * 60
}
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let month = i64::from(month);
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}
/// 调用方持有全局锁。沿用记录已有的前缀；没有时按创建时间取一个未被占用的。
fn pin_asset_stem(dir: &Path, workspace: &Workspace, id: &str) -> Result<String> {
    let pin = workspace
        .pins
        .iter()
        .find(|pin| pin.id == id)
        .ok_or(tr!("贴图记录不存在", "The pin record doesn't exist"))?;
    if let Some(stem) = [&pin.image_file, &pin.original_file]
        .into_iter()
        .flatten()
        .find_map(|name| asset_stem(name))
    {
        return Ok(stem.to_string());
    }
    allocate_pin_stem(dir, workspace, id)
}
/// 调用方持有全局锁；分配与提交在同一把锁内，清单与磁盘都没用过的前缀即可用。
fn allocate_pin_stem(dir: &Path, workspace: &Workspace, id: &str) -> Result<String> {
    let mut used: HashSet<String> = workspace
        .pins
        .iter()
        .flat_map(|pin| [&pin.image_file, &pin.original_file])
        .flatten()
        .filter_map(|name| asset_stem(name).map(String::from))
        .collect();
    if let Ok(entries) = std::fs::read_dir(dir.join(PINS_DIR)) {
        for entry in entries.flatten() {
            if let Some(stem) = asset_stem(&entry.file_name().to_string_lossy()) {
                used.insert(stem.to_string());
            }
        }
    }
    let base = pin_stem_base(id);
    (1..=9999)
        .map(|index| {
            if index == 1 {
                base.clone()
            } else {
                format!("{base}-{index}")
            }
        })
        .find(|stem| !used.contains(stem))
        .ok_or_else(|| "贴图资源文件名已用尽".into())
}
/// 调用方持有全局锁；取磁盘上该前缀与角色最小的空闲版本号，平时在本名与 .v2 之间交替。
fn next_asset_name(dir: &Path, stem: &str, original: bool) -> Result<String> {
    std::fs::create_dir_all(dir.join(PINS_DIR))?;
    let mut taken = HashSet::new();
    for entry in std::fs::read_dir(dir.join(PINS_DIR))?.flatten() {
        if let Some((other, role, revision)) = parse_asset(&entry.file_name().to_string_lossy()) {
            if other == stem && role == original {
                taken.insert(revision);
            }
        }
    }
    let revision = (1..=999_999)
        .find(|revision| !taken.contains(revision))
        .ok_or("贴图资源版本号已用尽")?;
    let role = if original { ".original" } else { "" };
    Ok(if revision == 1 {
        format!("{stem}{role}.png")
    } else {
        format!("{stem}{role}.v{revision}.png")
    })
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
fn unique_suffix() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:032x}-{:08x}-{:016x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", unique_suffix())
}
fn controlled_id(id: &str, prefix: &str) -> bool {
    let Some(suffix) = id
        .strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix('-'))
    else {
        return false;
    };
    let parts: Vec<_> = suffix.split('-').collect();
    parts.len() == 3
        && parts.iter().zip([32, 8, 16]).all(|(value, length)| {
            value.len() == length
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}
/// 只接受本程序生成的贴图编号格式。
pub(crate) fn valid_pin_id(id: &str) -> bool {
    controlled_id(id, "pin")
}
fn validate_pin_id(id: &str) -> Result<()> {
    if !controlled_id(id, "pin") {
        return Err("贴图记录标识无效".into());
    }
    Ok(())
}
fn asset_id(filename: &str) -> Option<&str> {
    let (id, suffix) = filename.strip_suffix(".png")?.split_once("-asset-")?;
    if !controlled_id(id, "pin") || !controlled_id(&format!("asset-{suffix}"), "asset") {
        return None;
    }
    Some(id)
}
/// 只识别自有资源或清单的暂存名，不按 .tmp 扩展名清理其它文件。
fn controlled_temp_stem(filename: &str) -> Option<&str> {
    filename
        .strip_suffix(".tmp")
        .filter(|stem| asset_id(stem).is_some() || controlled_id(stem, "workspace"))
}
fn asset_path_in(dir: &Path, filename: &str) -> Result<PathBuf> {
    if parse_asset(filename).is_some() {
        return Ok(dir.join(PINS_DIR).join(filename));
    }
    // 旧版平铺资源名只在迁移完成前出现。
    if asset_id(filename).is_none() {
        return Err("贴图资源必须是工作区自有 PNG 文件名".into());
    }
    Ok(dir.join(filename))
}
fn lease_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_pin_id(id)?;
    Ok(runtime_dir(dir).join(format!("{id}.lease")))
}
fn new_lease(dir: &Path, id: &str) -> Result<File> {
    ensure_runtime_dir(dir)?;
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(0)
        .open(lease_path(dir, id)?)?)
}
fn acquire_lease(dir: &Path, id: &str) -> Result<File> {
    ensure_runtime_dir(dir)?;
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(lease_path(dir, id)?)?)
}
fn lease_is_live(dir: &Path, id: &str) -> bool {
    let Ok(path) = lease_path(dir, id) else {
        return true;
    };
    // 旧版本进程的租约仍在根目录；任一处被独占都视为使用中。
    [path, dir.join(format!("{id}.lease"))]
        .into_iter()
        .any(|path| {
            match OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(0)
                .open(path)
            {
                Ok(_) => false,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => true,
            }
        })
}
fn global_lock(dir: &Path) -> Result<File> {
    global_lock_with_layout(dir, true)
}
fn global_lock_with_layout(dir: &Path, migrate_layout: bool) -> Result<File> {
    std::fs::create_dir_all(dir)?;
    let runtime = ensure_runtime_dir(dir)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(runtime.join("workspace.lock"))?;
    // 多个贴图同时登记时短暂占锁属常态：有界短重试（约 300ms），仍被占用再报本地化错误。
    for attempt in 0..20 {
        match file.try_lock() {
            Ok(()) => {
                // 迁移失败不阻断事务：旧版资源名仍可直接读取，下次加锁再试。
                if migrate_layout {
                    if let Err(error) = migrate_flat_layout(dir) {
                        eprintln!("Pin workspace layout migration failed: {error}");
                    }
                }
                return Ok(file);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                if attempt < 19 {
                    std::thread::sleep(std::time::Duration::from_millis(15));
                }
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    Err(trf!(
        "贴图工作区正被另一个 {} 进程使用，请稍后重试",
        "The pin workspace is in use by another {} process; try again shortly",
        crate::brand::NAME
    )
    .into())
}
/// 调用方持有全局锁。旧版平铺布局（资源、租约、临时文件都在根目录）迁到
/// pins/<创建时间>.png 与 .runtime/：先硬链接到新位置并写清单，再删旧文件，
/// 任一步中断都至少保留一份清单可用的资源。
fn migrate_flat_layout(dir: &Path) -> Result<()> {
    let legacy_files: Vec<String> = std::fs::read_dir(dir)?
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            asset_id(name).is_some()
                || name
                    .strip_suffix(".lease")
                    .is_some_and(|id| controlled_id(id, "pin"))
                || controlled_temp_stem(name).is_some()
                || name == "workspace.lock"
        })
        .collect();
    if legacy_files.is_empty() {
        return Ok(());
    }
    // 旧品牌补图尚不确定时不改资源引用或回收旧布局；普通事务仍可继续。
    relocation::reconcile_locked(dir)?;
    let mut workspace = match load_in(dir) {
        Ok(workspace) => workspace,
        // 清单不可读时保持原样，由正常读取路径报告错误。
        Err(_) => return Ok(()),
    };
    // load_in可能刚导入JSON；补图仍失败时不得把旧资源引用改名或回收。
    relocation::reconcile_locked(dir)?;
    let legacy_pins: Vec<String> = workspace
        .pins
        .iter()
        .filter(|pin| {
            [&pin.image_file, &pin.original_file]
                .into_iter()
                .flatten()
                .any(|name| parse_asset(name).is_none())
        })
        .map(|pin| pin.id.clone())
        .collect();
    if !legacy_pins.is_empty() {
        for id in &legacy_pins {
            let stem = allocate_pin_stem(dir, &workspace, id)?;
            let pin = workspace
                .pins
                .iter_mut()
                .find(|pin| &pin.id == id)
                .ok_or("贴图记录不存在")?;
            for (field, role) in [
                (&mut pin.image_file, ""),
                (&mut pin.original_file, ".original"),
            ] {
                let Some(old) = field.clone().filter(|name| parse_asset(name).is_none()) else {
                    continue;
                };
                let new = format!("{stem}{role}.png");
                std::fs::create_dir_all(dir.join(PINS_DIR))?;
                let (from, to) = (asset_path_in(dir, &old)?, asset_path_in(dir, &new)?);
                match std::fs::hard_link(&from, &to) {
                    Ok(()) => {}
                    // 原本就缺失的资源保持缺失，记录照常换成新名字。
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => {
                        std::fs::copy(&from, &to)?;
                    }
                }
                *field = Some(new);
            }
        }
        bump_revision(&mut workspace)?;
        write_manifest(dir, &encoded_manifest(&workspace)?)?;
    }
    for name in legacy_files {
        let path = dir.join(&name);
        let stale = || {
            std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|time| {
                    SystemTime::now().duration_since(time).unwrap_or_default()
                        > Duration::from_secs(3600)
                })
        };
        let remove = if let Some(id) = asset_id(&name) {
            !lease_is_live(dir, id)
        } else if let Some(stem) = controlled_temp_stem(&name) {
            stale() && !asset_id(stem).is_some_and(|id| lease_is_live(dir, id))
        } else if let Some(id) = name.strip_suffix(".lease") {
            !lease_is_live(dir, id)
        } else {
            // 旧锁仍被旧版本进程占用时删除会失败，留待下次。
            true
        };
        if remove {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}
fn load_in(dir: &Path) -> Result<Workspace> {
    let workspace = db::load(dir)?;
    // 普通元数据提交和新图保存不因其它记录的旧来源暂不可读而失效。
    let _ = relocation::reclaim_allowed(dir);
    Ok(workspace)
}
fn load_unlocked_in(dir: &Path) -> Result<Workspace> {
    let workspace = db::load(dir)?;
    relocation::reconcile(dir)?;
    Ok(workspace)
}
fn load_legacy_in(dir: &Path) -> Result<Workspace> {
    crate::image_collection::ensure_local_file_or_missing(&manifest(dir))?;
    let file = match File::open(manifest(dir)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Workspace::default());
        }
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("贴图工作区清单超过大小限制".into());
    }
    let wire: WireWorkspace = serde_json::from_slice(&bytes)?;
    let workspace = wire.into_model()?;
    validate_workspace(&workspace)?;
    Ok(workspace)
}
fn validate_name(name: &str, limit: usize, allow_empty: bool) -> Result<()> {
    if (!allow_empty && name.trim().is_empty())
        || name.chars().count() > limit
        || name.chars().any(char::is_control)
    {
        return Err(tr!(
            "名称不能为空或包含控制字符，且不能过长",
            "Names must be non-empty, without control characters, and not too long"
        )
        .into());
    }
    Ok(())
}
fn validate_state(state: &PinState, p: Placement) -> Result<()> {
    if !state.zoom.is_finite()
        || !state.normal_zoom.is_finite()
        || !(0.0001..=8.).contains(&state.zoom)
        || !(0.0001..=8.).contains(&state.normal_zoom)
        || state.opacity < 26
        || state.adjustments.turns > 3
        || !(-100..=100).contains(&state.adjustments.brightness)
        || p.width == 0
        || p.height == 0
        || p.width > 30000
        || p.height > 30000
    {
        return Err("贴图窗口位置、缩放或图像处理参数无效".into());
    }
    Ok(())
}
fn validate_window_sizing(sizing: PinWindowSizing, p: Placement) -> Result<()> {
    if sizing == PinWindowSizing::ExactCapture
        && (p.width > crate::pin_model::MAX_PIN_WINDOW_SIDE
            || p.height > crate::pin_model::MAX_PIN_WINDOW_SIDE)
    {
        return Err("截图窗口政策的尺寸超过 16384 像素限制".into());
    }
    Ok(())
}
fn validate_workspace(workspace: &Workspace) -> Result<()> {
    if workspace.groups.is_empty()
        || workspace.groups.len() > MAX_GROUPS
        || workspace.pins.len() > MAX_ACTIVE + MAX_CLOSED
    {
        return Err("贴图工作区记录数量超过限制".into());
    }
    if workspace
        .pins
        .iter()
        .filter(|pin| pin.status == PinStatus::Active)
        .count()
        > MAX_ACTIVE
        || workspace
            .pins
            .iter()
            .filter(|pin| pin.status == PinStatus::Closed)
            .count()
            > MAX_CLOSED
    {
        return Err("贴图活动或关闭记录数量超过限制".into());
    }
    let mut group_ids = HashSet::new();
    for group in &workspace.groups {
        if group.id != DEFAULT_GROUP_ID && !controlled_id(&group.id, "group") {
            return Err("贴图分组标识无效".into());
        }
        if !group_ids.insert(group.id.as_str()) {
            return Err("贴图分组标识重复".into());
        }
        validate_name(&group.name, 64, false)?;
    }
    if !group_ids.contains(DEFAULT_GROUP_ID)
        || !group_ids.contains(workspace.current_group.as_str())
    {
        return Err("默认分组或当前分组缺失".into());
    }
    let mut pins = HashSet::new();
    let mut stems = HashSet::new();
    for pin in &workspace.pins {
        validate_pin_id(&pin.id)?;
        if !pins.insert(pin.id.as_str()) || !group_ids.contains(pin.group_id.as_str()) {
            return Err("贴图标识重复或所属分组不存在".into());
        }
        validate_name(&pin.title, 256, true)?;
        validate_state(&pin.state, pin.placement)?;
        validate_window_sizing(pin.window_sizing, pin.placement)?;
        if let Some(doc) = &pin.text_document {
            doc.validate()?;
            if pin.image_file.is_none() {
                return Err("纯文字贴图缺少兼容图片快照".into());
            }
        }
        match (
            &pin.presentation_style,
            &pin.original_file,
            &pin.raster_document,
        ) {
            (style, Some(_), Some(document)) => {
                if let Some(style) = style {
                    validate_presentation_style(*style)?;
                }
                document.validate()?;
                if pin.image_file.is_none() || pin.text_document.is_some() {
                    return Err("截图贴图缺少兼容快照或混入文字来源".into());
                }
            }
            (None, None, None) => {}
            _ => return Err("截图贴图原图、外观与文档元数据不完整".into()),
        }
        if let Some(history) = &pin.processing_history {
            let document = pin.raster_document.as_ref().ok_or("处理历史缺少图片文档")?;
            if pin.text_document.is_some() || pin.original_file.is_none() {
                return Err("处理历史不能借用纯文字或缺失原图来源".into());
            }
            let (original, _) = history.dimensions();
            history.validate(pin.state.adjustments, original, document.size())?;
        }
        let mut pin_stem = None;
        for name in [&pin.image_file, &pin.original_file].into_iter().flatten() {
            if let Some(stem) = asset_stem(name) {
                if pin_stem.replace(stem).is_some_and(|other| other != stem) {
                    return Err("同一贴图的资源文件名前缀不一致".into());
                }
            } else if asset_id(name) != Some(pin.id.as_str()) {
                return Err("贴图资源文件与记录标识不匹配".into());
            }
        }
        if let Some(stem) = pin_stem {
            if !stems.insert(stem) {
                return Err("多个贴图共用同一个资源文件名前缀".into());
            }
        }
    }
    Ok(())
}
fn bump_revision(workspace: &mut Workspace) -> Result<()> {
    workspace.revision = workspace
        .revision
        .checked_add(1)
        .ok_or("工作区版本计数溢出")?;
    Ok(())
}
fn encoded_manifest_bytes(workspace: &Workspace) -> Result<Vec<u8>> {
    validate_workspace(workspace)?;
    Ok(serde_json::to_vec(&WireWorkspace::from(workspace))?)
}
fn encoded_manifest(workspace: &Workspace) -> Result<Vec<u8>> {
    let bytes = encoded_manifest_bytes(workspace)?;
    for pin in &workspace.pins {
        if db::document_bytes(pin)? > MAX_DOCUMENT_BYTES {
            return Err("单张贴图文档超过大小限制".into());
        }
    }
    Ok(bytes)
}
/// 文字容量只裁当前记录两栈的最远步骤，坏语义及 current 超限直接失败。
fn encoded_text_manifest(workspace: &mut Workspace, id: &str) -> Result<Vec<u8>> {
    loop {
        encoded_manifest_bytes(workspace)?;
        let pin = workspace
            .pins
            .iter()
            .find(|pin| pin.id == id)
            .ok_or("贴图记录不存在")?;
        if db::document_bytes(pin)? <= MAX_DOCUMENT_BYTES {
            return encoded_manifest(workspace);
        }
        let trimmed = workspace
            .pins
            .iter_mut()
            .find(|pin| pin.id == id)
            .and_then(|pin| pin.text_document.as_mut())
            .is_some_and(WireTextDocument::trim_oldest_history);
        if !trimmed {
            return Err("单张文字贴图文档超过大小限制".into());
        }
    }
}
/// 每张图片贴图独立限制文档容量；其它记录和当前文档内容不因历史容量被改写。
fn encoded_raster_manifest(workspace: &mut Workspace, id: &str) -> Result<Vec<u8>> {
    loop {
        // 验证和序列化错误直接传播，只有实际字节超限才允许裁剪历史。
        encoded_manifest_bytes(workspace)?;
        let current = workspace
            .pins
            .iter()
            .find(|pin| pin.id == id)
            .ok_or("当前贴图记录不存在")?;
        if db::document_bytes(current)? <= MAX_DOCUMENT_BYTES {
            return encoded_manifest(workspace);
        }
        let pin = workspace
            .pins
            .iter_mut()
            .find(|pin| pin.id == id)
            .ok_or("当前贴图记录不存在")?;
        let trimmed = pin
            .processing_history
            .as_mut()
            .is_some_and(WireProcessingHistory::trim_oldest_history)
            || pin
                .raster_document
                .as_mut()
                .is_some_and(WireRasterDocument::trim_oldest_history);
        if !trimmed {
            return Err("单张图片贴图文档超过大小限制".into());
        }
    }
}
fn commit(
    dir: &Path,
    workspace: &mut Workspace,
    protected: Option<&str>,
    prepared: Option<(&str, u64)>,
) -> Result<()> {
    prune(dir, workspace, protected, prepared)?;
    bump_revision(workspace)?;
    write_manifest(dir, &encoded_manifest(workspace)?)?;
    cleanup_orphans(dir, workspace, None);
    Ok(())
}
fn prune(
    dir: &Path,
    workspace: &mut Workspace,
    protected: Option<&str>,
    prepared: Option<(&str, u64)>,
) -> Result<()> {
    prune_with_budget(dir, workspace, protected, prepared, MAX_BYTES)
}
fn prune_with_budget(
    dir: &Path,
    workspace: &mut Workspace,
    protected: Option<&str>,
    prepared: Option<(&str, u64)>,
    budget: u64,
) -> Result<()> {
    let prepared: Vec<_> = prepared.into_iter().collect();
    prune_with_assets(dir, workspace, protected, &prepared, budget)
}
fn prune_record_limits(
    dir: &Path,
    workspace: &mut Workspace,
    protected: Option<&str>,
) -> Result<()> {
    prune_record_limits_with_reclaim(dir, workspace, protected, relocation::reclaim_allowed(dir))
}
fn prune_record_limits_with_reclaim(
    dir: &Path,
    workspace: &mut Workspace,
    protected: Option<&str>,
    reclaim: bool,
) -> Result<()> {
    let removable = |pin: &PinEntry| {
        reclaim && Some(pin.id.as_str()) != protected && !lease_is_live(dir, &pin.id)
    };
    let now = now_ms();
    workspace.pins.retain(|pin| {
        !(removable(pin)
            && (pin.image_file.is_none()
                || (pin.status == PinStatus::Closed
                    && now.saturating_sub(pin.updated_ms) > CLOSED_AGE_MS)))
    });
    if workspace
        .pins
        .iter()
        .filter(|pin| pin.status == PinStatus::Active)
        .count()
        > MAX_ACTIVE
    {
        return Err(tr!(
            "最多同时保存 50 张活动贴图",
            "At most 50 active pins can be kept"
        )
        .into());
    }
    while workspace
        .pins
        .iter()
        .filter(|pin| pin.status == PinStatus::Closed)
        .count()
        > MAX_CLOSED
    {
        let candidate = workspace
            .pins
            .iter()
            .enumerate()
            .filter(|(_, pin)| pin.status == PinStatus::Closed && removable(pin))
            .min_by_key(|(_, pin)| pin.updated_ms)
            .map(|(index, _)| index)
            .ok_or("关闭贴图记录均正在使用，暂时无法修剪")?;
        workspace.pins.remove(candidate);
    }
    Ok(())
}
fn prune_with_assets(
    dir: &Path,
    workspace: &mut Workspace,
    protected: Option<&str>,
    prepared: &[(&str, u64)],
    budget: u64,
) -> Result<()> {
    let reclaim = relocation::reclaim_allowed(dir);
    prune_record_limits_with_reclaim(dir, workspace, protected, reclaim)?;
    let removable = |pin: &PinEntry| {
        reclaim && Some(pin.id.as_str()) != protected && !lease_is_live(dir, &pin.id)
    };
    loop {
        let metadata = encoded_manifest(workspace)?.len() as u64;
        let mut total = metadata;
        let mut assets = HashSet::new();
        for pin in &workspace.pins {
            for name in [&pin.image_file, &pin.original_file].into_iter().flatten() {
                if !assets.insert(name.as_str()) {
                    continue;
                }
                let bytes = if let Some((_, bytes)) =
                    prepared.iter().find(|(filename, _)| *filename == name)
                {
                    *bytes
                } else {
                    match std::fs::metadata(asset_path_in(dir, name)?) {
                        Ok(metadata) => metadata.len(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                        Err(error) => return Err(error.into()),
                    }
                };
                total = total.checked_add(bytes).ok_or("工作区容量统计溢出")?;
            }
        }
        if total <= budget {
            return Ok(());
        }
        let candidate = workspace
            .pins
            .iter()
            .enumerate()
            .filter(|(_, pin)| removable(pin))
            .min_by_key(|(_, pin)| (pin.status == PinStatus::Active, pin.updated_ms))
            .map(|(index, _)| index)
            .ok_or(tr!(
                "贴图工作区超过 256 MiB，当前使用中的贴图不能修剪",
                "The pin workspace exceeds 256 MiB and pins in use can't be trimmed"
            ))?;
        workspace.pins.remove(candidate);
    }
}
fn write_manifest(dir: &Path, bytes: &[u8]) -> Result<()> {
    db::write(dir, bytes)
}
fn move_new(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        core::PCWSTR,
        Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH},
    };
    // std::fs 支持长路径；直接传普通 Win32 路径会在深层数据目录退回 MAX_PATH。
    // canonicalize 保留 Windows verbatim/UNC 前缀；新目标只解析已存在的父目录。
    let from = std::fs::canonicalize(from)?;
    let to = std::fs::canonicalize(to.parent().ok_or("贴图资源没有父目录")?)?
        .join(to.file_name().ok_or("贴图资源没有文件名")?);
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|error| std::io::Error::from_raw_os_error((error.code().0 as u32 & 0xffff) as i32))?;
    Ok(())
}
/// 调用方持有全局锁。资源只在清单提交后由同一把锁回收，
/// 因此 pins/ 里没有被任何记录占用前缀的图片都是删除或中断遗留，可直接回收。
fn cleanup_orphans(dir: &Path, workspace: &Workspace, replaced: Option<&str>) {
    if !relocation::reclaim_allowed(dir) {
        return;
    }
    if let Ok(files) = std::fs::read_dir(dir.join(PINS_DIR)) {
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            let Some(stem) = asset_stem(&name) else {
                continue;
            };
            if !file.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let owner = workspace.pins.iter().find(|pin| {
                [&pin.image_file, &pin.original_file]
                    .into_iter()
                    .flatten()
                    .any(|asset| asset_stem(asset) == Some(stem))
            });
            let remove = match owner {
                None => true,
                Some(owner) => {
                    owner.image_file.as_deref() != Some(name.as_str())
                        && owner.original_file.as_deref() != Some(name.as_str())
                        && (replaced == Some(owner.id.as_str()) || !lease_is_live(dir, &owner.id))
                }
            };
            if remove {
                let _ = std::fs::remove_file(file.path());
            }
        }
    }
    let Ok(files) = std::fs::read_dir(runtime_dir(dir)) else {
        return;
    };
    for entry in files.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(id) = name
            .strip_suffix(".lease")
            .filter(|id| controlled_id(id, "pin"))
        {
            if !workspace.pins.iter().any(|pin| pin.id == id) && !lease_is_live(dir, id) {
                let _ = std::fs::remove_file(entry.path());
            }
        } else if let Some(stem) = controlled_temp_stem(&name) {
            // 资源暂存名以贴图标识开头：持有租约的窗口可能正在编码，不回收。
            let owner_live = asset_id(stem).is_some_and(|id| lease_is_live(dir, id));
            if !owner_live
                && entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|time| {
                        SystemTime::now().duration_since(time).unwrap_or_default()
                            > Duration::from_secs(3600)
                    })
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

// 外部领域类型不 derive Serde；磁盘字段名称与枚举值固定，不依赖 Rust 成员命名。
#[derive(Serialize, Deserialize, PartialEq)]
struct WireWorkspace {
    #[serde(rename = "version")]
    version: u32,
    #[serde(rename = "groups")]
    groups: Vec<WireGroup>,
    #[serde(rename = "currentGroup")]
    current_group: String,
    #[serde(rename = "restoreOnStartup")]
    restore_on_startup: bool,
    #[serde(rename = "pins")]
    pins: Vec<WirePin>,
    #[serde(rename = "revision")]
    revision: u64,
}
#[derive(Serialize, Deserialize, PartialEq)]
struct WireGroup {
    #[serde(rename = "id")]
    id: String,
    #[serde(rename = "name")]
    name: String,
}
fn deserialize_processing_history<'de, D>(
    d: D,
) -> std::result::Result<Option<WireProcessingHistory>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    WireProcessingHistory::deserialize(d).map(Some)
}
#[derive(Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum WireWindowSizing {
    #[default]
    General,
    ExactCapture,
}
impl From<PinWindowSizing> for WireWindowSizing {
    fn from(sizing: PinWindowSizing) -> Self {
        match sizing {
            PinWindowSizing::General => Self::General,
            PinWindowSizing::ExactCapture => Self::ExactCapture,
        }
    }
}
impl From<WireWindowSizing> for PinWindowSizing {
    fn from(sizing: WireWindowSizing) -> Self {
        match sizing {
            WireWindowSizing::General => Self::General,
            WireWindowSizing::ExactCapture => Self::ExactCapture,
        }
    }
}
#[derive(Serialize, Deserialize, PartialEq)]
struct WirePin {
    #[serde(rename = "id")]
    id: String,
    #[serde(rename = "groupId")]
    group_id: String,
    #[serde(rename = "title")]
    title: String,
    #[serde(rename = "status")]
    status: String,
    #[serde(rename = "hidden")]
    hidden: bool,
    #[serde(rename = "clickThrough", default)]
    click_through: bool,
    #[serde(rename = "placement")]
    placement: WirePlacement,
    #[serde(rename = "windowSizing", default)]
    window_sizing: WireWindowSizing,
    #[serde(rename = "state")]
    state: WireState,
    #[serde(rename = "imageFile")]
    image_file: Option<String>,
    #[serde(
        rename = "textDocument",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    text_document: Option<WireTextDocument>,
    #[serde(
        rename = "presentationStyle",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    presentation_style: Option<SelectionFinishStyle>,
    #[serde(
        rename = "originalFile",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    original_file: Option<String>,
    #[serde(
        rename = "rasterDocument",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    raster_document: Option<WireRasterDocument>,
    #[serde(
        rename = "processingHistory",
        default,
        deserialize_with = "deserialize_processing_history",
        skip_serializing_if = "Option::is_none"
    )]
    processing_history: Option<WireProcessingHistory>,
    #[serde(rename = "updatedMs")]
    updated_ms: u64,
}
#[derive(Serialize, Deserialize, PartialEq)]
struct WirePlacement {
    #[serde(rename = "x")]
    x: i32,
    #[serde(rename = "y")]
    y: i32,
    #[serde(rename = "width")]
    width: u32,
    #[serde(rename = "height")]
    height: u32,
}
#[derive(Serialize, Deserialize, PartialEq)]
struct WireState {
    #[serde(rename = "zoom")]
    zoom: f32,
    #[serde(rename = "opacity")]
    opacity: u8,
    #[serde(rename = "locked")]
    locked: bool,
    #[serde(rename = "topmost")]
    topmost: bool,
    #[serde(
        rename = "showDropShadow",
        default = "crate::pin_model::default_shadow_enabled"
    )]
    shadow_enabled: bool,
    #[serde(rename = "thumbnail")]
    thumbnail: bool,
    #[serde(rename = "normalZoom")]
    normal_zoom: f32,
    #[serde(rename = "turns")]
    turns: u8,
    #[serde(rename = "flipX")]
    flip_x: bool,
    #[serde(rename = "flipY")]
    flip_y: bool,
    #[serde(rename = "grayscale")]
    grayscale: bool,
    #[serde(rename = "invert")]
    invert: bool,
    #[serde(rename = "brightness")]
    brightness: i16,
}
impl From<&PinState> for WireState {
    fn from(s: &PinState) -> Self {
        Self {
            zoom: s.zoom,
            opacity: s.opacity,
            locked: s.locked,
            topmost: s.topmost,
            shadow_enabled: s.shadow_enabled,
            thumbnail: s.thumbnail,
            normal_zoom: s.normal_zoom,
            turns: s.adjustments.turns,
            flip_x: s.adjustments.flip_x,
            flip_y: s.adjustments.flip_y,
            grayscale: s.adjustments.grayscale,
            invert: s.adjustments.invert,
            brightness: s.adjustments.brightness,
        }
    }
}
impl From<&Workspace> for WireWorkspace {
    fn from(w: &Workspace) -> Self {
        Self {
            version: LAYOUT_VERSION,
            groups: w
                .groups
                .iter()
                .map(|g| WireGroup {
                    id: g.id.clone(),
                    name: g.name.clone(),
                })
                .collect(),
            current_group: w.current_group.clone(),
            restore_on_startup: w.restore_on_startup,
            revision: w.revision,
            pins: w
                .pins
                .iter()
                .map(|p| WirePin {
                    id: p.id.clone(),
                    group_id: p.group_id.clone(),
                    title: p.title.clone(),
                    status: match p.status {
                        PinStatus::Active => "active",
                        PinStatus::Closed => "closed",
                    }
                    .into(),
                    hidden: p.hidden,
                    click_through: p.click_through,
                    placement: WirePlacement {
                        x: p.placement.x,
                        y: p.placement.y,
                        width: p.placement.width,
                        height: p.placement.height,
                    },
                    window_sizing: p.window_sizing.into(),
                    state: WireState::from(&p.state),
                    image_file: p.image_file.clone(),
                    text_document: p.text_document.clone(),
                    presentation_style: p.presentation_style,
                    original_file: p.original_file.clone(),
                    raster_document: p.raster_document.clone(),
                    processing_history: p.processing_history.clone(),
                    updated_ms: p.updated_ms,
                })
                .collect(),
        }
    }
}
impl WireWorkspace {
    fn into_model(self) -> Result<Workspace> {
        // version 1 是旧版平铺布局，读入后在首次加锁时迁移。
        if !(1..=LAYOUT_VERSION).contains(&self.version) {
            return Err(tr!(
                "不支持的贴图工作区版本，已保留原文件",
                "Unsupported pin workspace version; the original file is kept"
            )
            .into());
        }
        let pins = self
            .pins
            .into_iter()
            .map(|p| -> Result<PinEntry> {
                let s = p.state;
                Ok(PinEntry {
                    id: p.id,
                    group_id: p.group_id,
                    title: p.title,
                    status: match p.status.as_str() {
                        "active" => PinStatus::Active,
                        "closed" => PinStatus::Closed,
                        _ => return Err("贴图状态无效".into()),
                    },
                    hidden: p.hidden,
                    click_through: p.click_through,
                    placement: Placement {
                        x: p.placement.x,
                        y: p.placement.y,
                        width: p.placement.width,
                        height: p.placement.height,
                    },
                    window_sizing: p.window_sizing.into(),
                    state: PinState {
                        zoom: s.zoom,
                        opacity: s.opacity,
                        locked: s.locked,
                        topmost: s.topmost,
                        shadow_enabled: s.shadow_enabled,
                        thumbnail: s.thumbnail,
                        normal_zoom: s.normal_zoom,
                        adjustments: Adjustments {
                            turns: s.turns,
                            flip_x: s.flip_x,
                            flip_y: s.flip_y,
                            grayscale: s.grayscale,
                            invert: s.invert,
                            brightness: s.brightness,
                        },
                    },
                    image_file: p.image_file,
                    text_document: p.text_document,
                    presentation_style: p.presentation_style,
                    original_file: p.original_file,
                    raster_document: p.raster_document,
                    processing_history: p.processing_history,
                    updated_ms: p.updated_ms,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Workspace {
            groups: self
                .groups
                .into_iter()
                .map(|g| Group {
                    id: g.id,
                    name: g.name,
                })
                .collect(),
            current_group: self.current_group,
            restore_on_startup: self.restore_on_startup,
            pins,
            revision: self.revision,
        })
    }
}

#[cfg(test)]
#[path = "pin_mixed_text_host_workspace_tests.rs"]
mod pin_mixed_text_host_workspace_tests;

#[cfg(test)]
#[path = "pin_capture_workspace_tests.rs"]
mod capture_workspace_tests;

#[cfg(test)]
#[path = "pin_workspace_control_store_tests.rs"]
mod control_store_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_names_are_localized_only_at_display() {
        let default = Workspace::default();
        assert_eq!(default.groups[0].name, "默认");
        assert_eq!(
            default.groups[0].display_name(),
            crate::tr!("默认", "Default")
        );
        let renamed = Group {
            id: DEFAULT_GROUP_ID.into(),
            name: "常用".into(),
        };
        assert_eq!(renamed.display_name(), "常用");
        assert_eq!(
            display_pin_title(""),
            crate::tr!("未命名贴图", "Untitled pin")
        );
        assert_eq!(display_pin_title("贴图"), crate::tr!("贴图", "Pin"));
        assert_eq!(display_pin_title("报表"), "报表");
    }

    #[test]
    fn pin_workspace_shadow_is_per_window_and_missing_wire_defaults_enabled() {
        let mut workspace = Workspace::default();
        for (index, shadow_enabled) in [false, true].into_iter().enumerate() {
            workspace.pins.push(PinEntry {
                id: format!("shadow-{index}"),
                group_id: workspace.current_group.clone(),
                title: String::new(),
                status: PinStatus::Closed,
                hidden: false,
                click_through: false,
                placement: Placement {
                    x: -1921,
                    y: -701,
                    width: 22,
                    height: 100,
                },
                window_sizing: PinWindowSizing::General,
                state: PinState {
                    shadow_enabled,
                    ..Default::default()
                },
                image_file: None,
                text_document: None,
                presentation_style: None,
                original_file: None,
                raster_document: None,
                processing_history: None,
                updated_ms: 0,
            });
        }
        let mut wire = serde_json::to_value(WireWorkspace::from(&workspace)).unwrap();
        assert_eq!(wire["pins"][0]["state"]["showDropShadow"], false);
        assert_eq!(wire["pins"][1]["state"]["showDropShadow"], true);
        let restored = serde_json::from_value::<WireWorkspace>(wire.clone())
            .unwrap()
            .into_model()
            .unwrap();
        for (before, after) in workspace.pins.iter().zip(&restored.pins) {
            assert_eq!(before.state, after.state);
            assert_eq!(before.placement, after.placement);
        }
        wire["pins"][0]["state"]
            .as_object_mut()
            .unwrap()
            .remove("showDropShadow");
        let restored = serde_json::from_value::<WireWorkspace>(wire.clone())
            .unwrap()
            .into_model()
            .unwrap();
        assert!(restored.pins[0].state.shadow_enabled);
        assert!(restored.pins[1].state.shadow_enabled);
        wire["pins"][0]["state"]["showDropShadow"] = serde_json::json!("false");
        assert!(serde_json::from_value::<WireWorkspace>(wire).is_err());
    }

    struct TestDir(PathBuf, Vec<PathBuf>);
    impl TestDir {
        fn new() -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/qa")
                .join(new_id("pin-workspace-model"));
            std::fs::create_dir_all(&path).unwrap();
            Self(path, Vec::new())
        }
        fn deep() -> Self {
            use std::os::windows::ffi::OsStrExt;
            let mut dir = Self::new();
            while dir.0.as_os_str().encode_wide().count() <= 300 {
                dir.1.push(dir.0.clone());
                dir.0 = dir.0.join("shared-snapshot-deep-path-".repeat(3));
            }
            std::fs::create_dir_all(&dir.0).unwrap();
            dir
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            // 只递归删除工作区自有的两个子目录，其余按平铺文件删除，不跟随链接。
            for sub in [PINS_DIR, RUNTIME_DIR] {
                let _ = std::fs::remove_dir_all(self.0.join(sub));
            }
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&self.0);
            // 深路径测试只逐层删除本次创建的空目录，不递归删除或跟随链接。
            for parent in self.1.iter().rev() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
    fn placement() -> Placement {
        Placement {
            x: -240,
            y: 120,
            width: 96,
            height: 72,
        }
    }
    fn image() -> Image {
        Image {
            width: 2,
            height: 2,
            bgra: vec![
                8, 32, 96, 128, 16, 64, 128, 255, 0, 0, 0, 0, 48, 80, 112, 192,
            ],
            readback_ms: 0.,
        }
    }
    fn raster_snapshot() -> (Image, Image, PinState, RasterSnapshotMetadata) {
        use crate::{
            annotation::{Annotation, Point, Style, Tool},
            editor::Editor,
            geometry::Rect,
            scene::{PixelPlane, Scene, SourcePixels},
        };
        let raw = Image {
            width: 9,
            height: 7,
            bgra: [20, 40, 80, 128].repeat(9 * 7),
            readback_ms: 0.,
        };
        let state = PinState {
            zoom: 1.25,
            adjustments: Adjustments {
                turns: 1,
                flip_y: true,
                brightness: 13,
                ..Default::default()
            },
            ..Default::default()
        };
        let current = state.adjustments.apply(&raw).unwrap();
        let full = Rect::full(current.width, current.height);
        let mut editor = Editor::new(full, Some(full));
        let mut mark = Annotation::new(
            Tool::Rectangle,
            Point { x: 1., y: 2. },
            Style {
                filled: true,
                color: 0x102030,
                opacity: 1.,
                ..Default::default()
            },
        );
        mark.update(Point { x: 5., y: 7. });
        editor.document.insert(mark);
        let document =
            WireRasterDocument::from_editor(&editor, (current.width, current.height)).unwrap();
        let source = SourcePixels {
            fill: None,
            canvas: full,
            planes: vec![PixelPlane {
                rect: full,
                bgra: &current.bgra,
            }],
            overlay: None,
        };
        let scene = Scene::prepare(&editor.document_annotations(), &source);
        assert!(scene.errors.is_empty());
        let mut canonical = source.read(full);
        scene.try_apply(&mut canonical, full).unwrap();
        let metadata = RasterSnapshotMetadata {
            original_size: (9, 7),
            presentation_style: Some(SelectionFinishStyle {
                corner_enabled: true,
                corner_radius: 3.,
                shadow_enabled: true,
                shadow_blur: 5.,
                border_enabled: true,
                border_thickness: 2.,
                ..Default::default()
            }),
            document,
            processing_history: None,
        };
        (raw, canonical, state, metadata)
    }
    #[test]
    fn raster_workspace_dual_assets_restore_raw_once_and_preserve_marks_style_and_processing() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let (raw, canonical, state, metadata) = raster_snapshot();
        let style = metadata.presentation_style;
        let document = metadata.document.clone();
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &session.id,
            &state,
            placement(),
            PinStatus::Closed,
            metadata,
            |path| {
                export::save_png_pixels(canonical.width, canonical.height, &canonical.bgra, path)
            },
            |path| export::save_png_pixels(raw.width, raw.height, &raw.bgra, path),
        )
        .unwrap();
        let entry = load_in(&dir.0).unwrap().pins.remove(0);
        assert_eq!(entry.state, state);
        assert_eq!(entry.presentation_style, style);
        assert_eq!(entry.raster_document, Some(document.clone()));
        let stored = export::read_image(
            &asset_path_in(&dir.0, entry.image_file.as_deref().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(stored.bgra, canonical.bgra);
        assert_eq!((stored.width, stored.height), (7, 9));
        assert_ne!(stored.bgra, raw.bgra);
        let id = session.id.clone();
        drop(session);
        let (_lease, restored, entry) = Session::claim_in(&dir.0, &id).unwrap();
        assert_eq!((restored.width, restored.height), (9, 7));
        assert_eq!(restored.bgra, raw.bgra);
        let processed = entry.state.adjustments.apply(&restored).unwrap();
        let full = crate::geometry::Rect::full(processed.width, processed.height);
        let mut editor = crate::editor::Editor::new(full, Some(full));
        entry
            .raster_document
            .unwrap()
            .restore_into(&mut editor)
            .unwrap();
        assert_eq!(editor.document.items().len(), 1);
        assert_eq!(processed.bgra[..4], [53, 73, 113, 128]);
        assert_eq!(restored.bgra[..4], [20, 40, 80, 128]);
    }
    #[test]
    fn raster_workspace_preserves_inside_border_through_save_and_restore() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let (raw, canonical, state, mut metadata) = raster_snapshot();
        metadata.presentation_style.as_mut().unwrap().border_inside = true;
        let style = metadata.presentation_style;
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &session.id,
            &state,
            placement(),
            PinStatus::Closed,
            metadata,
            |path| {
                export::save_png_pixels(canonical.width, canonical.height, &canonical.bgra, path)
            },
            |path| export::save_png_pixels(raw.width, raw.height, &raw.bgra, path),
        )
        .unwrap();
        assert_eq!(load_in(&dir.0).unwrap().pins[0].presentation_style, style);
        let id = session.id.clone();
        drop(session);
        let (_lease, restored, entry) = Session::claim_in(&dir.0, &id).unwrap();
        assert_eq!(entry.presentation_style, style);
        assert!(entry.presentation_style.unwrap().border_inside);
        assert_eq!(restored.bgra, raw.bgra, "内边框不能烘焙到处理原图");
    }
    #[test]
    fn ordinary_raster_workspace_keeps_raw_and_document_without_presentation_style() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let (raw, canonical, state, mut metadata) = raster_snapshot();
        metadata.presentation_style = None;
        let document = metadata.document.clone();
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &session.id,
            &state,
            placement(),
            PinStatus::Closed,
            metadata,
            |path| {
                export::save_png_pixels(canonical.width, canonical.height, &canonical.bgra, path)
            },
            |path| export::save_png_pixels(raw.width, raw.height, &raw.bgra, path),
        )
        .unwrap();
        let id = session.id.clone();
        drop(session);
        let (_lease, restored, entry) = Session::claim_in(&dir.0, &id).unwrap();
        assert!(entry.presentation_style.is_none());
        assert_eq!(entry.raster_document, Some(document));
        assert!(entry.original_file.is_some());
        assert_eq!(restored.bgra, raw.bgra, "恢复原图，不把标注烘焙两遍");
    }
    #[test]
    fn raster_close_at_twenty_closed_records_prunes_oldest_before_manifest_history_check() {
        use crate::{editor::Editor, geometry::Rect};
        // 两种真实 raster 保存入口：普通图片无外观，截图图片有外观。
        for styled in [false, true] {
            let dir = TestDir::new();
            for _ in 0..MAX_CLOSED {
                drop(register(&dir.0, PinStatus::Closed));
            }
            let mut workspace = load_in(&dir.0).unwrap();
            assert_eq!(workspace.pins.len(), MAX_CLOSED);
            // 固定唯一最旧记录，仍在七天保留期内，不靠系统睡眠决定顺序。
            workspace.pins[0].updated_ms = now_ms().saturating_sub(60_000);
            let oldest = workspace.pins[0].clone();
            let oldest_path = asset_path_in(&dir.0, oldest.image_file.as_deref().unwrap()).unwrap();
            let preserved: Vec<_> = workspace
                .pins
                .iter()
                .skip(1)
                .map(|pin| {
                    let path = asset_path_in(&dir.0, pin.image_file.as_deref().unwrap()).unwrap();
                    (pin.clone(), path.clone(), std::fs::read(path).unwrap())
                })
                .collect();
            write_manifest(&dir.0, &encoded_manifest(&workspace).unwrap()).unwrap();
            let session = register(&dir.0, PinStatus::Active);
            let (raw, canonical, state, mut metadata) = raster_snapshot();
            if !styled {
                metadata.presentation_style = None;
            }
            let style = metadata.presentation_style;
            let document = metadata.document.clone();
            let current_id = session.id.clone();
            // 调用生产实际双资产保存函数，不直接调用容量 helper 或替代重放算法。
            save_raster_snapshot_with_encoders_in(
                &dir.0,
                &current_id,
                &state,
                placement(),
                PinStatus::Closed,
                metadata,
                |path| {
                    export::save_png_pixels(
                        canonical.width,
                        canonical.height,
                        &canonical.bgra,
                        path,
                    )
                },
                |path| export::save_png_pixels(raw.width, raw.height, &raw.bgra, path),
            )
            .unwrap();
            let saved = load_in(&dir.0).unwrap();
            assert_eq!(saved.pins.len(), MAX_CLOSED);
            assert!(saved.pins.iter().all(|pin| pin.status == PinStatus::Closed));
            assert!(!saved.pins.iter().any(|pin| pin.id == oldest.id));
            assert!(
                !oldest_path.exists(),
                "既有最旧关闭记录及自有资产按原政策回收"
            );
            for (before, path, bytes) in preserved {
                let after = saved.pins.iter().find(|pin| pin.id == before.id).unwrap();
                assert_eq!(after.state, before.state);
                assert_eq!(after.placement, before.placement);
                assert_eq!(after.updated_ms, before.updated_ms);
                assert_eq!(after.raster_document, before.raster_document);
                assert_eq!(after.image_file, before.image_file);
                assert_eq!(after.original_file, before.original_file);
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
            let current = saved.pins.iter().find(|pin| pin.id == current_id).unwrap();
            assert_eq!(current.presentation_style, style);
            assert_eq!(current.raster_document, Some(document.clone()));
            assert_eq!(current.state, state);
            drop(session);
            let (_lease, restored, entry) = Session::claim_in(&dir.0, &current_id).unwrap();
            assert_eq!(restored.bgra, raw.bgra);
            let (width, height) = document.size();
            let full = Rect::full(width, height);
            let mut editor = Editor::new(full, Some(full));
            entry
                .raster_document
                .as_ref()
                .unwrap()
                .restore_into(&mut editor)
                .unwrap();
            let marks = editor.document.items().to_vec();
            assert!(editor.document.can_undo());
            editor.document.undo();
            assert!(editor.document.items().is_empty());
            assert!(editor.document.can_redo());
            editor.document.redo();
            assert_eq!(editor.document.items(), marks.as_slice());
        }
    }
    #[test]
    fn raster_document_budget_keeps_history_when_other_records_fill_the_library() {
        use crate::{
            annotation::{Annotation, Point, Style, Tool},
            editor::Editor,
            geometry::Rect,
        };
        fn entry(document: WireRasterDocument) -> PinEntry {
            let id = new_id("pin");
            PinEntry {
                image_file: Some(format!("{id}-asset-{}.png", unique_suffix())),
                original_file: Some(format!("{id}-asset-{}.png", unique_suffix())),
                id,
                group_id: DEFAULT_GROUP_ID.into(),
                title: "历史容量模型".into(),
                status: PinStatus::Active,
                hidden: false,
                click_through: false,
                placement: placement(),
                window_sizing: PinWindowSizing::General,
                state: PinState::default(),
                text_document: None,
                presentation_style: None,
                raster_document: Some(document),
                processing_history: None,
                updated_ms: now_ms(),
            }
        }
        fn text(bytes: usize) -> Annotation {
            let mut mark = Annotation::new(Tool::Text, Point { x: 1., y: 1. }, Style::default());
            mark.update(Point { x: 6., y: 5. });
            mark.text = "A\n".repeat(bytes / 2);
            mark
        }
        let full = Rect::full(9, 7);
        let mut first = Editor::new(full, Some(full));
        let id = first.document.insert(text(150 * 1024));
        let mut current = text(2);
        current.text = "current".into();
        first.document.replace(id, current);
        let first_doc = WireRasterDocument::from_editor(&first, (9, 7)).unwrap();
        let mut second = Editor::new(full, Some(full));
        second.document.insert(text(200 * 1024));
        let mut rectangle =
            Annotation::new(Tool::Rectangle, Point { x: 1., y: 1. }, Style::default());
        rectangle.update(Point { x: 6., y: 5. });
        second.document.insert(rectangle);
        let second_doc = WireRasterDocument::from_editor(&second, (9, 7)).unwrap();
        let second_bytes = serde_json::to_vec(&second_doc).unwrap();
        let mut workspace = Workspace {
            pins: vec![entry(first_doc), entry(second_doc.clone())],
            ..Default::default()
        };
        let first_id = workspace.pins[0].id.clone();
        let before: serde_json::Value =
            serde_json::to_value(workspace.pins[0].raster_document.as_ref().unwrap()).unwrap();
        assert!(encoded_manifest_bytes(&workspace).unwrap().len() as u64 > MAX_MANIFEST_BYTES);
        let encoded = encoded_raster_manifest(&mut workspace, &first_id).unwrap();
        assert!(encoded.len() as u64 > MAX_MANIFEST_BYTES);
        assert!(workspace
            .pins
            .iter()
            .all(|pin| db::document_bytes(pin).unwrap() <= MAX_DOCUMENT_BYTES));
        assert_eq!(
            serde_json::to_vec(workspace.pins[1].raster_document.as_ref().unwrap()).unwrap(),
            second_bytes,
            "其它贴图文档和历史字节不变"
        );
        assert_eq!(workspace.pins[1].raster_document, Some(second_doc));
        let after: serde_json::Value =
            serde_json::to_value(workspace.pins[0].raster_document.as_ref().unwrap()).unwrap();
        for field in ["objects", "crop", "mask"] {
            assert_eq!(after[field], before[field], "容量裁剪只作用于历史");
        }
        for field in ["ids", "nextId", "nextGroup"] {
            assert_eq!(
                after["history"][field], before["history"][field],
                "保留对象和分配器身份"
            );
        }
        assert_eq!(after["history"], before["history"]);

        // 单张文档的当前内容超限仍须拒绝，不能借其它记录的剩余容量保存。
        let mut oversized = workspace.clone();
        let mut document = after.clone();
        document["objects"][0]["text"] = serde_json::json!("");
        document.as_object_mut().unwrap().remove("history");
        // 单个矢量文档合法，加入数据库文档包外壳后才超过每条记录的上限。
        oversized.pins[0].raster_document = Some(serde_json::from_value(document.clone()).unwrap());
        let text_bytes = MAX_DOCUMENT_BYTES + 1 - db::document_bytes(&oversized.pins[0]).unwrap();
        document["objects"][0]["text"] = serde_json::json!(format!(
            "{}{}",
            "\u{0001}".repeat(text_bytes / 6),
            "a".repeat(text_bytes % 6)
        ));
        let document: WireRasterDocument = serde_json::from_value(document).unwrap();
        document.validate().unwrap();
        oversized.pins[0].raster_document = Some(document.clone());
        assert_eq!(
            db::document_bytes(&oversized.pins[0]).unwrap(),
            MAX_DOCUMENT_BYTES + 1
        );
        assert!(encoded_raster_manifest(&mut oversized, &first_id).is_err());
        assert_eq!(oversized.pins[0].raster_document, Some(document));

        // 不得把验证错误当容量错误，进而删掉一个本来有效的历史。
        let mut invalid = Workspace {
            pins: vec![
                entry(WireRasterDocument::from_editor(&first, (9, 7)).unwrap()),
                workspace.pins[1].clone(),
            ],
            ..Default::default()
        };
        invalid.pins[0].presentation_style = Some(SelectionFinishStyle {
            border_thickness: -1.,
            ..Default::default()
        });
        let old = invalid.pins[0].raster_document.clone();
        let id = invalid.pins[0].id.clone();
        assert!(encoded_raster_manifest(&mut invalid, &id).is_err());
        assert_eq!(invalid.pins[0].raster_document, old);
    }
    #[test]
    fn raster_workspace_failed_second_encoder_rolls_back_both_assets_and_metadata() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let (raw, canonical, state, metadata) = raster_snapshot();
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &session.id,
            &state,
            placement(),
            PinStatus::Active,
            metadata,
            |path| {
                export::save_png_pixels(canonical.width, canonical.height, &canonical.bgra, path)
            },
            |path| export::save_png_pixels(raw.width, raw.height, &raw.bgra, path),
        )
        .unwrap();
        let before = load_in(&dir.0).unwrap();
        let (raw, _canonical, mut state, mut metadata) = raster_snapshot();
        state.zoom = 2.;
        metadata.presentation_style.as_mut().unwrap().border_color = 0xffaabbcc;
        assert!(save_raster_snapshot_with_encoders_in(
            &dir.0,
            &session.id,
            &state,
            placement(),
            PinStatus::Closed,
            metadata,
            |_| Err("canonical PNG编码模拟失败".into()),
            |path| export::save_png_pixels(raw.width, raw.height, &raw.bgra, path)
        )
        .is_err());
        let after = load_in(&dir.0).unwrap();
        assert!(WireWorkspace::from(&before) == WireWorkspace::from(&after));
        let assets = workspace_assets(&dir.0).len();
        assert_eq!(assets, 2);
    }
    #[test]
    fn raster_workspace_state_only_updates_keep_raw_and_document_and_legacy_save_clears_them() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let (raw, canonical, mut state, metadata) = raster_snapshot();
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &session.id,
            &state,
            placement(),
            PinStatus::Active,
            metadata,
            |path| {
                export::save_png_pixels(canonical.width, canonical.height, &canonical.bgra, path)
            },
            |path| export::save_png_pixels(raw.width, raw.height, &raw.bgra, path),
        )
        .unwrap();
        let before = load_in(&dir.0).unwrap().pins.remove(0);
        state.zoom = 0.75;
        update_state_in(
            &dir.0,
            &session.id,
            &state,
            Placement {
                x: -1931,
                y: -721,
                ..placement()
            },
        )
        .unwrap();
        let after = load_in(&dir.0).unwrap().pins.remove(0);
        assert_eq!(after.original_file, before.original_file);
        assert_eq!(after.image_file, before.image_file);
        assert_eq!(after.raster_document, before.raster_document);
        assert_eq!(after.presentation_style, before.presentation_style);
        assert_eq!(after.state.adjustments, state.adjustments);
        let old_original = asset_path_in(&dir.0, after.original_file.as_deref().unwrap()).unwrap();
        state.adjustments = Default::default();
        save_snapshot_in(
            &dir.0,
            &session.id,
            &canonical,
            &state,
            placement(),
            PinStatus::Closed,
        )
        .unwrap();
        let ordinary = load_in(&dir.0).unwrap().pins.remove(0);
        assert!(ordinary.original_file.is_none());
        assert!(ordinary.presentation_style.is_none());
        assert!(ordinary.raster_document.is_none());
        assert!(!old_original.exists());
    }
    #[test]
    fn nested_workspace_real_save_load_state_update_and_bad_chain_preserve_atomic_metadata() {
        use crate::{
            annotation::{Annotation, Point, Style, Tool},
            editor::Editor,
            geometry::Rect,
            scene::{PixelPlane, Scene, SourcePixels},
        };
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let canvas = Rect::full(64, 48);
        let raw = Image {
            width: 64,
            height: 48,
            bgra: [20, 40, 80, 255].repeat(64 * 48),
            readback_ms: 0.,
        };
        let mut source = Editor::new(canvas, Some(canvas));
        let ids = source.document.insert_many(
            (0..3)
                .map(|index| {
                    let mut a = Annotation::new(
                        Tool::Rectangle,
                        Point {
                            x: 3. + index as f32 * 17.,
                            y: 5.,
                        },
                        Style {
                            color: 0x123456 + index,
                            filled: true,
                            ..Default::default()
                        },
                    );
                    a.update(Point {
                        x: 15. + index as f32 * 17.,
                        y: 24.,
                    });
                    a
                })
                .collect(),
        );
        assert!(source.document.set_group(&ids[..2], true));
        let child = source.document.items()[0].group.unwrap();
        assert!(source.document.set_group(&[ids[0], ids[2]], true));
        let root = source.document.items()[0].group.unwrap();
        let mut changed = source.document.get(ids[0]).unwrap().clone();
        changed.translate(0., 3.);
        source.document.replace(ids[0], changed);
        source.document.remove(ids[1]);
        source.document.undo();
        source.selected = Some(ids[0]);
        source.selection = source.expand_selection(&[ids[0]]);
        assert!(source.enter_group_scope());
        assert!(source.enter_group_scope());
        assert_eq!(source.group_scope_path(), vec![root, child]);
        let expected = source.document.bounded_history_snapshot().unwrap();
        let document = WireRasterDocument::from_editor(&source, (64, 48)).unwrap();
        let pixels = SourcePixels {
            canvas,
            fill: None,
            planes: vec![PixelPlane {
                rect: canvas,
                bgra: &raw.bgra,
            }],
            overlay: None,
        };
        let scene = Scene::prepare(&source.document_annotations(), &pixels);
        let mut canonical = pixels.read(canvas);
        scene.try_apply(&mut canonical, canvas).unwrap();
        let metadata = RasterSnapshotMetadata {
            original_size: (64, 48),
            presentation_style: None,
            document: document.clone(),
            processing_history: None,
        };
        let mut state = PinState::default();
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &session.id,
            &state,
            placement(),
            PinStatus::Active,
            metadata.clone(),
            |path| export::save_png_pixels(64, 48, &canonical.bgra, path),
            |path| export::save_png_pixels(64, 48, &raw.bgra, path),
        )
        .unwrap();
        let before = load_in(&dir.0).unwrap().pins.remove(0);
        assert_eq!(before.raster_document, Some(document.clone()));
        state.zoom = 0.75;
        let moved = Placement {
            x: -1931,
            y: -721,
            ..placement()
        };
        update_state_in(&dir.0, &session.id, &state, moved).unwrap();
        let after = load_in(&dir.0).unwrap().pins.remove(0);
        assert_eq!(after.raster_document, before.raster_document);
        assert_eq!(after.image_file, before.image_file);
        assert_eq!(after.original_file, before.original_file);
        assert_eq!(after.placement, moved);
        // 活动 lease 为 share_mode(0)，不读取自有锁；清单、PNG及其它自有文件逐字节核。
        let persisted_bytes = || {
            all_files(&dir.0)
                .into_iter()
                .filter(|name| !name.ends_with(".lease"))
                .map(|name| {
                    let bytes = std::fs::read(dir.0.join(&name)).unwrap();
                    (name, bytes)
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let files_before = persisted_bytes();
        let mut bad = serde_json::to_value(&document).unwrap();
        bad["objects"][0]["nestedGroups"] = serde_json::json!([root]);
        let mut invalid = metadata;
        invalid.document = serde_json::from_value(bad).unwrap();
        let called = std::cell::Cell::new(false);
        assert!(save_raster_snapshot_with_encoders_in(
            &dir.0,
            &session.id,
            &state,
            moved,
            PinStatus::Active,
            invalid,
            |_| {
                called.set(true);
                Err("无效嵌套组不得到达PNG编码".into())
            },
            |_| {
                called.set(true);
                Err("无效嵌套组不得到达原图编码".into())
            },
        )
        .is_err());
        assert!(!called.get());
        let files_after = persisted_bytes();
        assert_eq!(files_after, files_before);
        let id = session.id.clone();
        drop(session);
        let (_lease, restored_raw, entry) = Session::claim_in(&dir.0, &id).unwrap();
        assert_eq!(restored_raw.bgra, raw.bgra);
        let mut restored = Editor::new(canvas, Some(canvas));
        entry
            .raster_document
            .unwrap()
            .restore_into(&mut restored)
            .unwrap();
        assert!(restored.group_scope_path().is_empty());
        assert_eq!(
            restored.document.bounded_history_snapshot().unwrap(),
            expected
        );
        assert_eq!(restored.document.revision(), 0);
        assert_eq!(restored.document.edit_revision(), 0);
        restored.document.redo();
        source.document.redo();
        assert_eq!(restored.document.items(), source.document.items());
        restored.document.undo();
        source.document.undo();
        assert_eq!(restored.document.items(), source.document.items());
    }
    #[test]
    fn raster_workspace_capacity_counts_original_and_rejects_incomplete_metadata() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let (raw, canonical, state, metadata) = raster_snapshot();
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &session.id,
            &state,
            placement(),
            PinStatus::Active,
            metadata,
            |path| {
                export::save_png_pixels(canonical.width, canonical.height, &canonical.bgra, path)
            },
            |path| export::save_png_pixels(raw.width, raw.height, &raw.bgra, path),
        )
        .unwrap();
        let mut workspace = load_in(&dir.0).unwrap();
        let pin = &workspace.pins[0];
        let canonical_bytes =
            std::fs::metadata(asset_path_in(&dir.0, pin.image_file.as_deref().unwrap()).unwrap())
                .unwrap()
                .len();
        let metadata_bytes = encoded_manifest(&workspace).unwrap().len() as u64;
        assert!(prune_with_assets(
            &dir.0,
            &mut workspace,
            Some(&session.id),
            &[],
            metadata_bytes + canonical_bytes
        )
        .is_err());
        workspace.pins[0].original_file = None;
        assert!(encoded_manifest(&workspace).is_err());
        assert!(validate_presentation_style(SelectionFinishStyle {
            border_inside: true,
            ..Default::default()
        })
        .is_ok());
        assert!(validate_presentation_style(SelectionFinishStyle {
            border_thickness: -1.,
            ..Default::default()
        })
        .is_err());
    }
    fn register(dir: &Path, status: PinStatus) -> Session {
        let session = Session::register_in(dir, PinState::default(), placement()).unwrap();
        save_snapshot_in(
            dir,
            &session.id,
            &image(),
            &PinState::default(),
            placement(),
            status,
        )
        .unwrap();
        session
    }

    fn text_document() -> WireTextDocument {
        use crate::annotation::{Annotation, Point, Style, Tool};
        let full = crate::geometry::Rect::full(2, 2);
        let mut editor = crate::editor::Editor::new(full, Some(full));
        let mut a = Annotation::new(Tool::Text, Point { x: 0., y: 0. }, Style::default());
        a.update(Point { x: 2., y: 2. });
        a.text = "文本\t🙂\n下一行".into();
        editor.document.insert(a);
        WireTextDocument::from_text_editor(&editor, (2, 2), true).unwrap()
    }

    #[test]
    fn pin_workspace_text_document_roundtrip_and_legacy_snapshot_clear_stale_source() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let document = text_document();
        save_snapshot_with_text_document_in(
            &dir.0,
            &session.id,
            (2, 2),
            &PinState::default(),
            placement(),
            PinStatus::Closed,
            Some(document.clone()),
            |path| export::save_png(image(), path),
        )
        .unwrap();
        let saved = load_in(&dir.0).unwrap();
        assert_eq!(saved.pins[0].text_document.as_ref(), Some(&document));
        assert!(saved.pins[0].image_file.is_some());
        let mut old_wire = serde_json::to_value(WireWorkspace::from(&saved)).unwrap();
        old_wire["pins"][0]
            .as_object_mut()
            .unwrap()
            .remove("textDocument");
        let old = serde_json::from_value::<WireWorkspace>(old_wire)
            .unwrap()
            .into_model()
            .unwrap();
        assert!(old.pins[0].text_document.is_none());
        save_snapshot_in(
            &dir.0,
            &session.id,
            &image(),
            &PinState::default(),
            placement(),
            PinStatus::Closed,
        )
        .unwrap();
        assert!(load_in(&dir.0).unwrap().pins[0].text_document.is_none());
    }

    #[test]
    fn pin_workspace_text_document_failed_encode_retains_same_png_and_metadata() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        save_snapshot_with_text_document_in(
            &dir.0,
            &session.id,
            (2, 2),
            &PinState::default(),
            placement(),
            PinStatus::Active,
            Some(text_document()),
            |path| export::save_png(image(), path),
        )
        .unwrap();
        let before = load_in(&dir.0).unwrap();
        let mut changed = text_document();
        changed.width = 3;
        assert!(save_snapshot_with_text_document_in(
            &dir.0,
            &session.id,
            (3, 2),
            &PinState::default(),
            placement(),
            PinStatus::Closed,
            Some(changed),
            |_| Err("模拟PNG编码失败".into()),
        )
        .is_err());
        let after = load_in(&dir.0).unwrap();
        assert!(WireWorkspace::from(&before) == WireWorkspace::from(&after));
        assert_eq!(
            export::read_image(
                &asset_path_in(&dir.0, after.pins[0].image_file.as_ref().unwrap()).unwrap()
            )
            .unwrap()
            .bgra,
            image().bgra
        );
    }

    #[test]
    fn pin_workspace_text_document_dimension_or_adjustment_mismatch_never_encodes() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let before = load_in(&dir.0).unwrap();
        let calls = std::cell::Cell::new(0);
        for (size, state) in [
            ((3, 2), PinState::default()),
            (
                (2, 2),
                PinState {
                    adjustments: Adjustments {
                        invert: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
        ] {
            assert!(save_snapshot_with_text_document_in(
                &dir.0,
                &session.id,
                size,
                &state,
                placement(),
                PinStatus::Closed,
                Some(text_document()),
                |_| {
                    calls.set(calls.get() + 1);
                    Err("不应开始编码".into())
                },
            )
            .is_err());
        }
        assert_eq!(calls.get(), 0);
        assert!(WireWorkspace::from(&before) == WireWorkspace::from(&load_in(&dir.0).unwrap()));
    }

    #[test]
    fn registered_entry_matches_committed_group_and_keeps_lease_until_rollback() {
        let dir = TestDir::new();
        let before = apply_in(
            &dir.0,
            Action::Create {
                name: "截图测试".into(),
            },
        )
        .unwrap();
        let state = PinState {
            zoom: 1.25,
            opacity: 75,
            topmost: false,
            ..Default::default()
        };
        let (session, entry) =
            Session::register_with_entry_in(&dir.0, state.clone(), placement()).unwrap();
        let committed = load_in(&dir.0).unwrap();
        assert_eq!(committed.revision, before.revision + 1);
        assert_eq!(entry.id, session.id);
        assert_eq!(entry.group_id, before.current_group);
        assert_eq!(entry.placement, placement());
        assert_eq!(entry.state, state);
        assert_eq!(entry.status, PinStatus::Active);
        assert!(lease_is_live(&dir.0, &entry.id));
        let expected = Workspace {
            pins: vec![entry],
            ..committed.clone()
        };
        assert!(WireWorkspace::from(&expected) == WireWorkspace::from(&committed));
        drop(session);
        assert!(load_in(&dir.0).unwrap().pins.is_empty());
    }

    #[test]
    fn pin_workspace_manual_restore_ignores_startup_switch_and_record_status() {
        let dir = TestDir::new();
        let active = register(&dir.0, PinStatus::Active);
        let active_id = active.id.clone();
        let closed = register(&dir.0, PinStatus::Closed);
        let closed_id = closed.id.clone();
        drop(active);
        drop(closed);
        let workspace = apply_in(&dir.0, Action::SetRestore { enabled: false }).unwrap();
        assert!(restorable_in(&dir.0).unwrap().is_empty());
        assert_eq!(
            manual_restore_ids_in(&dir.0, &workspace),
            HashSet::from([active_id.clone(), closed_id])
        );
        let mut expected = workspace.clone();
        expected.revision += 1;
        let (claimed, decoded, entry) = Session::claim_in(&dir.0, &active_id).unwrap();
        assert_eq!(entry.status, PinStatus::Active);
        assert_eq!(decoded.bgra, image().bgra);
        assert_eq!(claimed.owner_generation, expected.revision);
        assert_eq!(
            encoded_manifest(&expected).unwrap(),
            encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
        );
        drop(claimed);
    }

    #[test]
    fn pin_workspace_manual_restore_respects_lease_resource_and_duplicate_claim() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let id = session.id.clone();
        let workspace = load_in(&dir.0).unwrap();
        assert!(manual_restore_ids_in(&dir.0, &workspace).is_empty());
        drop(session);
        assert_eq!(
            manual_restore_ids_in(&dir.0, &workspace),
            HashSet::from([id.clone()])
        );
        let (claimed, _, entry) = Session::claim_in(&dir.0, &id).unwrap();
        assert!(manual_restore_ids_in(&dir.0, &workspace).is_empty());
        assert!(Session::claim_in(&dir.0, &id).is_err());
        drop(claimed);
        assert_eq!(
            manual_restore_ids_in(&dir.0, &workspace),
            HashSet::from([id.clone()])
        );
        // 与正式快照编码相同，原生原子重命名须保留深层 QA 目录的 verbatim 前缀。
        let canonical_dir = std::fs::canonicalize(&dir.0).unwrap();
        let asset = asset_path_in(&canonical_dir, entry.image_file.as_deref().unwrap()).unwrap();
        std::fs::remove_file(&asset).unwrap();
        assert!(manual_restore_ids_in(&dir.0, &workspace).is_empty());
        assert!(Session::claim_in(&dir.0, &id).is_err());
        let mut expected = workspace.clone();
        // 首次合法 claim 和取得租约后发现资源丢失的 claim 各消耗一个新代次。
        expected.revision += 2;
        assert_eq!(
            encoded_manifest(&expected).unwrap(),
            encoded_manifest(&load_in(&dir.0).unwrap()).unwrap()
        );
        export::save_png(image(), &asset).unwrap();
        assert_eq!(
            manual_restore_ids_in(&dir.0, &workspace),
            HashSet::from([id])
        );
    }

    #[test]
    fn groups_visibility_and_order_survive_roundtrip() {
        let dir = TestDir::new();
        let first = register(&dir.0, PinStatus::Active);
        let first_id = first.id.clone();
        let workspace = apply_in(
            &dir.0,
            Action::Create {
                name: "资料".into(),
            },
        )
        .unwrap();
        let group = workspace.current_group;
        let second = register(&dir.0, PinStatus::Closed);
        apply_in(
            &dir.0,
            Action::SetHidden {
                id: first_id.clone(),
                hidden: true,
            },
        )
        .unwrap();
        apply_in(
            &dir.0,
            Action::SwitchGroup {
                id: DEFAULT_GROUP_ID.into(),
            },
        )
        .unwrap();
        assert!(
            load_in(&dir.0)
                .unwrap()
                .pins
                .iter()
                .find(|pin| pin.id == first_id)
                .unwrap()
                .hidden
        );
        apply_in(
            &dir.0,
            Action::MovePin {
                id: first_id.clone(),
                group_id: group.clone(),
            },
        )
        .unwrap();
        apply_in(
            &dir.0,
            Action::Reorder {
                ids: vec![group.clone(), DEFAULT_GROUP_ID.into()],
            },
        )
        .unwrap();
        apply_in(
            &dir.0,
            Action::Rename {
                id: DEFAULT_GROUP_ID.into(),
                name: "常用".into(),
            },
        )
        .unwrap();
        assert!(apply_in(
            &dir.0,
            Action::DeleteEmptyGroup {
                id: DEFAULT_GROUP_ID.into()
            }
        )
        .is_err());
        assert!(apply_in(&dir.0, Action::DeleteEmptyGroup { id: group.clone() }).is_err());
        drop(first);
        drop(second);
        let workspace = load_in(&dir.0).unwrap();
        assert_eq!(workspace.groups[0].id, group);
        assert_eq!(workspace.groups[1].name, "常用");
        assert_eq!(workspace.current_group, DEFAULT_GROUP_ID);
        let pin = workspace
            .pins
            .iter()
            .find(|pin| pin.id == first_id)
            .unwrap();
        assert!(pin.hidden);
        assert_eq!(pin.group_id, group);
        assert_eq!(pin.placement, placement());
        assert_eq!(pin.state, PinState::default());
        assert_eq!(restorable_in(&dir.0).unwrap().len(), 1);
    }

    #[test]
    fn failed_window_or_decode_does_not_consume_closed_record() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Closed);
        let id = session.id.clone();
        assert!(closed_in(&dir.0).unwrap().is_empty());
        assert!(Session::claim_in(&dir.0, &id).is_err());
        drop(session);
        assert_eq!(closed_in(&dir.0).unwrap().len(), 1);
        let (claimed, decoded, entry) = Session::claim_in(&dir.0, &id).unwrap();
        assert_eq!(entry.status, PinStatus::Closed);
        assert_eq!(decoded.bgra, image().bgra);
        assert!(Session::claim_in(&dir.0, &id).is_err());
        drop(claimed); // 模拟原生窗口创建失败。
        assert_eq!(closed_in(&dir.0).unwrap()[0].id, id);
        let path = asset_path_in(&dir.0, entry.image_file.as_deref().unwrap()).unwrap();
        let original = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"broken png").unwrap();
        assert!(Session::claim_in(&dir.0, &id).is_err());
        assert_eq!(closed_in(&dir.0).unwrap()[0].id, id);
        std::fs::write(&path, original).unwrap();
        let (claimed, _, _) = Session::claim_in(&dir.0, &id).unwrap();
        claimed.mark_active().unwrap();
        assert!(restorable_in(&dir.0).unwrap().is_empty());
        drop(claimed);
        assert_eq!(restorable_in(&dir.0).unwrap()[0].id, id);
        apply_in(&dir.0, Action::SetRestore { enabled: false }).unwrap();
        assert!(restorable_in(&dir.0).unwrap().is_empty());
    }

    #[test]
    fn close_order_is_persistent_and_not_changed_by_title() {
        let dir = TestDir::new();
        let first = register(&dir.0, PinStatus::Active);
        let second = register(&dir.0, PinStatus::Active);
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        apply_in(
            &dir.0,
            Action::Close {
                id: second_id.clone(),
            },
        )
        .unwrap();
        apply_in(
            &dir.0,
            Action::Close {
                id: first_id.clone(),
            },
        )
        .unwrap();
        apply_in(
            &dir.0,
            Action::SetTitle {
                id: second_id.clone(),
                title: "稍后改名".into(),
            },
        )
        .unwrap();
        drop(first);
        drop(second);
        let closed = closed_in(&dir.0).unwrap();
        assert_eq!(
            closed.iter().map(|pin| pin.id.as_str()).collect::<Vec<_>>(),
            vec![first_id.as_str(), second_id.as_str()]
        );
        assert_eq!(closed[1].title, "稍后改名");
        assert!(restorable_in(&dir.0).unwrap().is_empty());
    }

    #[test]
    fn clear_and_forget_respect_live_owners_and_remove_own_images() {
        let dir = TestDir::new();
        let active = register(&dir.0, PinStatus::Active);
        let live_closed = register(&dir.0, PinStatus::Closed);
        let closed = register(&dir.0, PinStatus::Closed);
        let closed_id = closed.id.clone();
        let workspace = load_in(&dir.0).unwrap();
        let closed_path = asset_path_in(
            &dir.0,
            workspace
                .pins
                .iter()
                .find(|pin| pin.id == closed_id)
                .unwrap()
                .image_file
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        let live_path = asset_path_in(
            &dir.0,
            workspace
                .pins
                .iter()
                .find(|pin| pin.id == live_closed.id)
                .unwrap()
                .image_file
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        drop(closed);
        clear_closed_in(&dir.0).unwrap();
        let workspace = load_in(&dir.0).unwrap();
        assert_eq!(workspace.pins.len(), 2);
        assert!(!closed_path.exists());
        assert!(live_path.exists());
        assert!(forget_in(&dir.0, &live_closed.id, false).is_err());
        live_closed.forget().unwrap();
        assert!(!live_path.exists());
        assert_eq!(load_in(&dir.0).unwrap().pins[0].id, active.id);
    }

    struct WriteFailure(rusqlite::Connection);
    impl WriteFailure {
        fn new(dir: &Path) -> Self {
            let connection = crate::storage_db::open_workspace(dir).unwrap();
            connection
                .execute_batch(
                    "CREATE TRIGGER test_reject_pin_commit BEFORE UPDATE ON pin_meta
                     BEGIN SELECT RAISE(FAIL, 'controlled pin database commit failure'); END;",
                )
                .unwrap();
            Self(connection)
        }
    }
    impl Drop for WriteFailure {
        fn drop(&mut self) {
            self.0
                .execute_batch("DROP TRIGGER test_reject_pin_commit")
                .unwrap();
        }
    }
    #[test]
    fn pin_workspace_consuming_encoder_releases_source_before_manifest_commit() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        struct Payload {
            image: Image,
            lock: Option<File>,
            dropped: Arc<AtomicUsize>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                drop(self.lock.take());
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }

        let dir = TestDir::new();
        let session = Session::register_in(&dir.0, PinState::default(), placement()).unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let payload = Arc::new(Payload {
            image: image(),
            // 真锁随 capture 一同释放：若提交时仍持源，try_lock 必定拒绝提交。
            lock: Some(global_lock(&dir.0).unwrap()),
            dropped: dropped.clone(),
        });
        let source = Arc::downgrade(&payload);
        let encode_calls = calls.clone();
        let encoding_dir = dir.0.clone();
        save_snapshot_with_encoder_in(
            &dir.0,
            &session.id,
            (2, 2),
            &PinState::default(),
            placement(),
            PinStatus::Closed,
            move |path| {
                encode_calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(payload.dropped.load(Ordering::SeqCst), 0);
                assert!(global_lock(&encoding_dir).is_err());
                export::save_png_pixels(
                    payload.image.width,
                    payload.image.height,
                    &payload.image.bgra,
                    path,
                )
            },
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(source.upgrade().is_none());
        let entry = load_in(&dir.0).unwrap().pins.remove(0);
        assert_eq!(entry.status, PinStatus::Closed);
        let path = asset_path_in(&dir.0, entry.image_file.as_deref().unwrap()).unwrap();
        let saved = export::read_image(&path).unwrap();
        assert_eq!((saved.width, saved.height), (2, 2));
        assert_eq!(saved.bgra, image().bgra);
    }

    #[test]
    fn pin_workspace_consuming_encoder_failures_keep_previous_asset_and_release_source() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        struct Payload {
            image: Image,
            dropped: Arc<AtomicUsize>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                self.dropped.fetch_add(1, Ordering::SeqCst);
            }
        }

        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let old_manifest = db::export_wire_bytes(&dir.0).unwrap();
        let entry = load_in(&dir.0).unwrap().pins.remove(0);
        let old_path = asset_path_in(&dir.0, entry.image_file.as_deref().unwrap()).unwrap();
        let old_png = std::fs::read(&old_path).unwrap();
        for fail_encoding in [true, false] {
            let dropped = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let payload = Arc::new(Payload {
                image: Image {
                    width: 2,
                    height: 2,
                    bgra: [150, 100, 50, 127].repeat(4),
                    readback_ms: 0.,
                },
                dropped: dropped.clone(),
            });
            let source = Arc::downgrade(&payload);
            let encode_calls = calls.clone();
            let rejected_commit = (!fail_encoding).then(|| WriteFailure::new(&dir.0));
            let result = save_snapshot_with_encoder_in(
                &dir.0,
                &session.id,
                (2, 2),
                &PinState::default(),
                placement(),
                PinStatus::Closed,
                move |path| {
                    encode_calls.fetch_add(1, Ordering::SeqCst);
                    export::save_png_pixels(
                        payload.image.width,
                        payload.image.height,
                        &payload.image.bgra,
                        path,
                    )?;
                    if fail_encoding {
                        Err("受控编码失败，已写出的临时 PNG 也必须回滚".into())
                    } else {
                        Ok(())
                    }
                },
            );
            drop(rejected_commit);
            let error = result.unwrap_err();
            if !fail_encoding {
                assert!(error
                    .to_string()
                    .contains("controlled pin database commit failure"));
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(dropped.load(Ordering::SeqCst), 1);
            assert!(source.upgrade().is_none());
            assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), old_manifest);
            assert_eq!(std::fs::read(&old_path).unwrap(), old_png);
            assert!(!all_files(&dir.0).iter().any(|name| name.ends_with(".tmp")));
            assert_eq!(workspace_assets(&dir.0).len(), 1);
        }
    }
    #[test]
    fn busy_or_failed_manifest_commit_keeps_previous_snapshot() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let old_manifest = db::export_wire_bytes(&dir.0).unwrap();
        let entry = load_in(&dir.0).unwrap().pins.remove(0);
        let old_path = asset_path_in(&dir.0, entry.image_file.as_deref().unwrap()).unwrap();
        let old_png = std::fs::read(&old_path).unwrap();
        let state = PinState {
            locked: true,
            ..Default::default()
        };
        let lock = global_lock(&dir.0).unwrap();
        assert!(save_snapshot_in(
            &dir.0,
            &session.id,
            &image(),
            &state,
            placement(),
            PinStatus::Closed
        )
        .is_err());
        drop(lock);
        assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), old_manifest);
        assert_eq!(std::fs::read(&old_path).unwrap(), old_png);
        let rejected_commit = WriteFailure::new(&dir.0);
        let error = save_snapshot_in(
            &dir.0,
            &session.id,
            &image(),
            &state,
            placement(),
            PinStatus::Closed,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("controlled pin database commit failure"));
        drop(rejected_commit);
        assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), old_manifest);
        assert_eq!(std::fs::read(&old_path).unwrap(), old_png);
        assert_eq!(workspace_assets(&dir.0).len(), 1);
        assert!(!all_files(&dir.0).iter().any(|name| name.ends_with(".tmp")));
        let invalid = PinState {
            zoom: f32::NAN,
            ..Default::default()
        };
        assert!(update_state_in(&dir.0, &session.id, &invalid, placement()).is_err());
        assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), old_manifest);
        save_snapshot_in(
            &dir.0,
            &session.id,
            &image(),
            &state,
            placement(),
            PinStatus::Active,
        )
        .unwrap();
        assert!(!old_path.exists()); // 自己的 live lease 不阻止替换后回收旧资源。
    }

    #[test]
    fn pin_workspace_shared_snapshot_streams_exact_png_and_rolls_back_failed_save() {
        use std::os::windows::ffi::OsStrExt;
        let dir = TestDir::deep();
        assert!(dir.0.as_os_str().encode_wide().count() > 300);
        let session = Session::register_in(&dir.0, PinState::default(), placement()).unwrap();
        let bgra = vec![
            41, 89, 153, 255, 33, 57, 121, 128, 73, 105, 137, 192, 25, 25, 25, 0, 200, 20, 100, 1,
            70, 90, 110, 254,
        ];
        let original = bgra.clone();
        save_snapshot_pixels_in(
            &dir.0,
            &session.id,
            (2, 3),
            &bgra,
            &PinState::default(),
            placement(),
            PinStatus::Active,
        )
        .unwrap();
        assert_eq!(bgra, original);
        let old_manifest = db::export_wire_bytes(&dir.0).unwrap();
        let entry = load_in(&dir.0).unwrap().pins.remove(0);
        let path = asset_path_in(&dir.0, entry.image_file.as_deref().unwrap()).unwrap();
        let png = std::fs::read(&path).unwrap();
        let rgba = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        assert_eq!(rgba.dimensions(), (2, 3));
        for (actual, source) in rgba.as_raw().chunks_exact(4).zip(bgra.chunks_exact(4)) {
            assert_eq!(actual, &[source[2], source[1], source[0], source[3]]);
        }
        // 包装旧 Image API 与共享 API 的 PNG 配置、像素和字节保持一致。
        let expected = export::encode_image(
            &Image {
                width: 2,
                height: 3,
                bgra: bgra.clone(),
                readback_ms: 0.,
            },
            Default::default(),
        )
        .unwrap();
        assert_eq!(png, expected);
        let replacement = [150, 100, 50, 255].repeat(6);
        let lock = global_lock(&dir.0).unwrap();
        assert!(save_snapshot_pixels_in(
            &dir.0,
            &session.id,
            (2, 3),
            &replacement,
            &PinState::default(),
            placement(),
            PinStatus::Closed,
        )
        .is_err());
        drop(lock);
        assert!(save_snapshot_pixels_in(
            &dir.0,
            &session.id,
            (2, 3),
            &bgra[..bgra.len() - 1],
            &PinState::default(),
            placement(),
            PinStatus::Closed,
        )
        .is_err());
        assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), old_manifest);
        assert_eq!(std::fs::read(&path).unwrap(), png);
        assert!(!all_files(&dir.0).iter().any(|name| name.ends_with(".tmp")));
        assert_eq!(bgra, original);
    }

    #[test]
    fn limits_and_cleanup_only_touch_controlled_assets() {
        let dir = TestDir::new();
        for index in 1..MAX_GROUPS {
            apply_in(
                &dir.0,
                Action::Create {
                    name: format!("组{index}"),
                },
            )
            .unwrap();
        }
        let before = db::export_wire_bytes(&dir.0).unwrap();
        assert!(apply_in(
            &dir.0,
            Action::Create {
                name: "超限".into()
            }
        )
        .is_err());
        assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
        let untouched = [
            "notes.txt",
            "unrelated.png",
            "pin-uncontrolled-asset-old.png",
        ];
        for filename in untouched {
            std::fs::write(dir.0.join(filename), b"keep").unwrap();
        }
        let mut oldest = None;
        for _ in 0..=MAX_CLOSED {
            let session = register(&dir.0, PinStatus::Closed);
            if oldest.is_none() {
                oldest = load_in(&dir.0).unwrap().pins[0].image_file.clone();
            }
            drop(session);
        }
        assert_eq!(closed_in(&dir.0).unwrap().len(), MAX_CLOSED);
        assert!(!asset_path_in(&dir.0, oldest.as_deref().unwrap())
            .unwrap()
            .exists());
        for filename in untouched {
            assert_eq!(std::fs::read(dir.0.join(filename)).unwrap(), b"keep");
        }
        assert!(asset_path_in(&dir.0, "../private.png").is_err());
        assert!(asset_path_in(&dir.0, "C:\\private.png").is_err());
        let mut workspace = load_in(&dir.0).unwrap();
        workspace.pins[0].updated_ms = 0;
        write_manifest(&dir.0, &encoded_manifest(&workspace).unwrap()).unwrap();
        apply_in(&dir.0, Action::SetRestore { enabled: false }).unwrap();
        assert_eq!(closed_in(&dir.0).unwrap().len(), MAX_CLOSED - 1);
    }

    #[test]
    fn active_limit_and_invalid_legacy_import_are_not_silently_overwritten() {
        let dir = TestDir::new();
        let mut sessions = Vec::new();
        for _ in 0..MAX_ACTIVE {
            sessions.push(Session::register_in(&dir.0, PinState::default(), placement()).unwrap());
        }
        let before = db::export_wire_bytes(&dir.0).unwrap();
        assert!(Session::register_in(&dir.0, PinState::default(), placement()).is_err());
        assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
        drop(sessions); // 未产生图片的失败/取消新窗口会回收登记。
        assert!(load_in(&dir.0).unwrap().pins.is_empty());
        let legacy = TestDir::new();
        let mut wire = WireWorkspace::from(&Workspace::default());
        wire.version = 999;
        let future = serde_json::to_vec(&wire).unwrap();
        std::fs::write(manifest(&legacy.0), &future).unwrap();
        assert!(load_in(&legacy.0).is_err());
        assert!(apply_in(&legacy.0, Action::SetRestore { enabled: false }).is_err());
        assert_eq!(std::fs::read(manifest(&legacy.0)).unwrap(), future);
        let excessive = vec![b' '; MAX_MANIFEST_BYTES as usize + 1];
        let oversized = TestDir::new();
        std::fs::write(manifest(&oversized.0), &excessive).unwrap();
        assert!(load_in(&oversized.0).is_err());
        assert_eq!(std::fs::read(manifest(&oversized.0)).unwrap(), excessive);
    }

    #[test]
    fn budget_includes_metadata_and_protects_current_and_live_records() {
        let dir = TestDir::new();
        let old = register(&dir.0, PinStatus::Active);
        let current = register(&dir.0, PinStatus::Active);
        let before = db::export_wire_bytes(&dir.0).unwrap();
        let original = load_in(&dir.0).unwrap();
        let total = encoded_manifest(&original).unwrap().len() as u64
            + original
                .pins
                .iter()
                .map(|pin| {
                    std::fs::metadata(
                        asset_path_in(&dir.0, pin.image_file.as_deref().unwrap()).unwrap(),
                    )
                    .unwrap()
                    .len()
                })
                .sum::<u64>();
        let mut workspace = original.clone();
        assert!(
            prune_with_budget(&dir.0, &mut workspace, Some(&current.id), None, total - 1).is_err()
        );
        assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
        drop(old);
        let mut workspace = original;
        prune_with_budget(&dir.0, &mut workspace, Some(&current.id), None, total - 1).unwrap();
        assert_eq!(workspace.pins.len(), 1);
        assert_eq!(workspace.pins[0].id, current.id);
        let filename = workspace.pins[0].image_file.clone().unwrap();
        assert!(prune_with_budget(
            &dir.0,
            &mut workspace,
            Some(&current.id),
            Some((&filename, MAX_BYTES)),
            MAX_BYTES
        )
        .is_err());
        assert_eq!(workspace.pins.len(), 1); // 不能把刚保存图片删掉再返回成功。
        assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), before);
    }

    fn text_host_entry_bytes(entry: &PinEntry) -> Vec<u8> {
        serde_json::to_vec(&WireWorkspace::from(&Workspace {
            pins: vec![entry.clone()],
            ..Default::default()
        }))
        .unwrap()
    }

    fn empty_current_text_history() -> (WireTextDocument, u64, u64) {
        use crate::{
            annotation::{Annotation, Point, Style, Tool},
            editor::Editor,
            geometry::Rect,
        };
        let full = Rect::full(2, 2);
        let mut editor = Editor::new(full, Some(full));
        let text = |body: &str| {
            let mut a = Annotation::new(
                Tool::Text,
                Point { x: 0., y: 0. },
                Style {
                    color: crate::pin_text_source::colors(true).1,
                    ..Default::default()
                },
            );
            a.update(Point { x: 2., y: 2. });
            a.text = body.into();
            a
        };
        let first = editor.document.insert(text("已删除 e\u{301}\r\n🙂"));
        editor.document.clear();
        let second = editor.document.insert(text("待重做\t原文"));
        editor.document.undo();
        (
            WireTextDocument::try_from_text_editor(&editor, (2, 2), true).unwrap(),
            first,
            second,
        )
    }

    #[test]
    fn text_workspace_save_claim_restores_empty_current_and_real_two_stacks() {
        use crate::{editor::Editor, geometry::Rect};
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let id = session.id.clone();
        let (document, first, second) = empty_current_text_history();
        save_snapshot_with_text_document_in(
            &dir.0,
            &id,
            (2, 2),
            &PinState::default(),
            placement(),
            PinStatus::Closed,
            Some(document.clone()),
            |path| export::save_png(image(), path),
        )
        .unwrap();
        drop(session);
        let (lease, preview, entry) = Session::claim_in(&dir.0, &id).unwrap();
        assert_eq!(preview.bgra, image().bgra, "兼容 PNG 保留但不是矢量来源");
        assert_eq!(entry.text_document, Some(document.clone()));
        let full = Rect::full(2, 2);
        let mut restored = Editor::new(full, Some(full));
        document.restore_into(&mut restored, false).unwrap();
        assert!(restored.document.items().is_empty());
        assert!(restored.document.can_undo() && restored.document.can_redo());
        restored.document.redo();
        assert_eq!(restored.document.items()[0].id, second);
        assert_eq!(restored.document.items()[0].annotation.text, "待重做\t原文");
        assert_eq!(
            restored.document.items()[0].annotation.style.color,
            crate::pin_text_source::colors(false).1
        );
        restored.document.undo();
        restored.document.undo();
        assert_eq!(restored.document.items()[0].id, first);
        assert_eq!(
            restored.document.items()[0].annotation.text,
            "已删除 e\u{301}\r\n🙂"
        );
        drop(lease);
        assert_eq!(
            load_in(&dir.0).unwrap().pins[0].text_document,
            Some(document),
            "claim 不消费原历史"
        );
    }

    #[test]
    fn text_close_at_twenty_records_prunes_policy_before_full_manifest_validation() {
        let dir = TestDir::new();
        for _ in 0..MAX_CLOSED {
            drop(register(&dir.0, PinStatus::Closed));
        }
        let mut before = load_in(&dir.0).unwrap();
        before.pins[0].updated_ms = now_ms().saturating_sub(60_000);
        let oldest = before.pins[0].id.clone();
        let preserved = before.pins[1..].to_vec();
        write_manifest(&dir.0, &encoded_manifest(&before).unwrap()).unwrap();
        let session = register(&dir.0, PinStatus::Active);
        let (document, _, _) = empty_current_text_history();
        save_snapshot_with_text_document_in(
            &dir.0,
            &session.id,
            (2, 2),
            &PinState::default(),
            placement(),
            PinStatus::Closed,
            Some(document.clone()),
            |path| export::save_png(image(), path),
        )
        .unwrap();
        let after = load_in(&dir.0).unwrap();
        assert_eq!(after.pins.len(), MAX_CLOSED);
        assert!(after.pins.iter().all(|p| p.status == PinStatus::Closed));
        assert!(!after.pins.iter().any(|p| p.id == oldest));
        for entry in preserved {
            assert_eq!(
                text_host_entry_bytes(after.pins.iter().find(|p| p.id == entry.id).unwrap()),
                text_host_entry_bytes(&entry)
            );
        }
        assert_eq!(
            after
                .pins
                .iter()
                .find(|p| p.id == session.id)
                .unwrap()
                .text_document,
            Some(document)
        );
    }

    #[test]
    fn text_workspace_independent_document_budgets_keep_history_and_failed_save_is_atomic() {
        use crate::{
            annotation::{Annotation, Point, Style, Tool},
            editor::Editor,
            geometry::Rect,
        };
        fn document(body: &str, replace: bool) -> WireTextDocument {
            let full = Rect::full(2, 2);
            let mut editor = Editor::new(full, Some(full));
            let mut a = Annotation::new(Tool::Text, Point { x: 0., y: 0. }, Style::default());
            a.update(Point { x: 2., y: 2. });
            a.text = body.into();
            let id = editor.document.insert(a.clone());
            if replace {
                a.text = "current".into();
                editor.document.replace(id, a);
            }
            WireTextDocument::try_from_text_editor(&editor, (2, 2), true).unwrap()
        }
        let dir = TestDir::new();
        let other = document(&"\u{0001}".repeat(40 * 1024), false);
        let mut others = Vec::new();
        for _ in 0..2 {
            let session = register(&dir.0, PinStatus::Active);
            save_snapshot_with_text_document_in(
                &dir.0,
                &session.id,
                (2, 2),
                &PinState::default(),
                placement(),
                PinStatus::Active,
                Some(other.clone()),
                |path| export::save_png(image(), path),
            )
            .unwrap();
            others.push(session);
        }
        let current = register(&dir.0, PinStatus::Active);
        let initial = document(&"\u{0001}".repeat(30_000), true);
        let initial_wire = serde_json::to_value(&initial).unwrap();
        let before = load_in(&dir.0).unwrap();
        let other_entries: Vec<_> = before
            .pins
            .iter()
            .filter(|p| p.id != current.id)
            .cloned()
            .collect();
        let mut proposed = before.clone();
        proposed
            .pins
            .iter_mut()
            .find(|p| p.id == current.id)
            .unwrap()
            .text_document = Some(initial.clone());
        assert!(encoded_manifest_bytes(&proposed).unwrap().len() as u64 > MAX_MANIFEST_BYTES);
        save_snapshot_with_text_document_in(
            &dir.0,
            &current.id,
            (2, 2),
            &PinState::default(),
            placement(),
            PinStatus::Active,
            Some(initial),
            |path| export::save_png(image(), path),
        )
        .unwrap();
        let after = load_in(&dir.0).unwrap();
        let after_bytes = encoded_manifest(&after).unwrap();
        assert!(after_bytes.len() as u64 > MAX_MANIFEST_BYTES);
        assert!(after
            .pins
            .iter()
            .all(|pin| db::document_bytes(pin).unwrap() <= MAX_DOCUMENT_BYTES));
        for entry in other_entries {
            assert_eq!(
                text_host_entry_bytes(after.pins.iter().find(|p| p.id == entry.id).unwrap()),
                text_host_entry_bytes(&entry),
                "其它记录及其历史不裁"
            );
        }
        let saved = after.pins.iter().find(|p| p.id == current.id).unwrap();
        let saved_wire = serde_json::to_value(saved.text_document.as_ref().unwrap()).unwrap();
        assert_eq!(saved_wire["objects"], initial_wire["objects"]);
        for field in ["ids", "nextId", "nextGroup"] {
            assert_eq!(saved_wire["history"][field], initial_wire["history"][field]);
        }
        assert_eq!(saved_wire["history"], initial_wire["history"]);
        let old_png = asset_path_in(&dir.0, saved.image_file.as_deref().unwrap()).unwrap();
        let png_bytes = std::fs::read(&old_png).unwrap();
        let files_before = all_files(&dir.0);
        // 有效文档进入真实数据库提交后失败，原文档、历史和资源保持不变。
        let rejected_commit = WriteFailure::new(&dir.0);
        let error = save_snapshot_with_text_document_in(
            &dir.0,
            &current.id,
            (2, 2),
            &PinState::default(),
            placement(),
            PinStatus::Closed,
            Some(other),
            |path| export::save_png(image(), path),
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("controlled pin database commit failure"));
        drop(rejected_commit);
        assert_eq!(
            encoded_manifest(&load_in(&dir.0).unwrap()).unwrap(),
            after_bytes
        );
        assert_eq!(std::fs::read(old_png).unwrap(), png_bytes);
        let files_after = all_files(&dir.0);
        assert_eq!(files_after, files_before, "失败仅移除本次临时资源");
        drop(others);
    }

    fn processing_host_fixture() -> (
        Image,
        Image,
        PinState,
        RasterSnapshotMetadata,
        crate::pin_model::ProcessingHistory,
    ) {
        use crate::pin_model::ProcessingHistory;
        let raw = Image {
            width: 3,
            height: 2,
            bgra: vec![
                4, 20, 60, 255, 8, 40, 80, 127, 12, 60, 100, 255, 16, 80, 120, 63, 20, 100, 140,
                255, 24, 120, 160, 255,
            ],
            readback_ms: 0.,
        };
        let mut current = Adjustments::default();
        let mut history = ProcessingHistory::default();
        assert!(history.change(
            &mut current,
            Adjustments {
                turns: 1,
                ..Default::default()
            }
        ));
        let next = Adjustments {
            brightness: 23,
            flip_x: true,
            ..current
        };
        assert!(history.change(&mut current, next));
        let next = Adjustments {
            invert: true,
            ..current
        };
        assert!(history.change(&mut current, next));
        assert!(history.undo(&mut current));
        let image = current.apply(&raw).unwrap();
        let wire = WireProcessingHistory::from_history(&history, current, (3, 2), (2, 3)).unwrap();
        let state = PinState {
            adjustments: current,
            ..Default::default()
        };
        let metadata = RasterSnapshotMetadata {
            original_size: (3, 2),
            presentation_style: None,
            document: WireRasterDocument::empty((2, 3)).unwrap(),
            processing_history: Some(wire),
        };
        (raw, image, state, metadata, history)
    }

    #[test]
    fn processing_workspace_png_claim_restores_rotation_color_and_both_parameter_stacks() {
        use crate::pin_model::ProcessingHistory;
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let id = session.id.clone();
        let (raw, image, state, metadata, mut source) = processing_host_fixture();
        let document = metadata.document.clone();
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &id,
            &state,
            placement(),
            PinStatus::Closed,
            metadata,
            |p| export::save_png_pixels(image.width, image.height, &image.bgra, p),
            |p| export::save_png_pixels(raw.width, raw.height, &raw.bgra, p),
        )
        .unwrap();
        let saved = load_in(&dir.0).unwrap().pins.remove(0);
        let canonical_path = asset_path_in(&dir.0, saved.image_file.as_deref().unwrap()).unwrap();
        let canonical = export::read_image(&canonical_path).unwrap();
        assert_eq!((canonical.width, canonical.height), (2, 3));
        assert_eq!(canonical.bgra, image.bgra);
        drop(session);
        export::save_png_pixels(raw.width, raw.height, &raw.bgra, &canonical_path).unwrap();
        assert!(
            Session::claim_in(&dir.0, &id).is_err(),
            "处理后真实PNG尺寸错配不安装、不消费"
        );
        assert!(load_in(&dir.0).unwrap().pins.iter().any(|p| p.id == id));
        export::save_png_pixels(image.width, image.height, &image.bgra, &canonical_path).unwrap();
        let (lease, restored_raw, entry) = Session::claim_in(&dir.0, &id).unwrap();
        assert_eq!((restored_raw.width, restored_raw.height), (3, 2));
        assert_eq!(restored_raw.bgra, raw.bgra, "原图保存一次，不烘焙处理结果");
        assert_eq!(entry.raster_document, Some(document));
        let mut restored = ProcessingHistory::default();
        entry
            .processing_history
            .as_ref()
            .unwrap()
            .restore_into(
                entry.state.adjustments,
                (restored_raw.width, restored_raw.height),
                (canonical.width, canonical.height),
                &mut restored,
            )
            .unwrap();
        let mut source_current = state.adjustments;
        let mut restored_current = entry.state.adjustments;
        assert_eq!(
            source.bounded_snapshot(source_current).unwrap(),
            restored.bounded_snapshot(restored_current).unwrap()
        );
        for undo in [false, true, true, true, false, false, false] {
            let source_changed = if undo {
                source.undo(&mut source_current)
            } else {
                source.redo(&mut source_current)
            };
            let restored_changed = if undo {
                restored.undo(&mut restored_current)
            } else {
                restored.redo(&mut restored_current)
            };
            assert!(source_changed && restored_changed);
            assert_eq!(
                source.bounded_snapshot(source_current).unwrap(),
                restored.bounded_snapshot(restored_current).unwrap()
            );
            let expected = source_current.apply(&raw).unwrap();
            let actual = restored_current.apply(&restored_raw).unwrap();
            assert_eq!(
                (actual.width, actual.height, &actual.bgra),
                (expected.width, expected.height, &expected.bgra)
            );
        }
        drop(lease);
        assert!(
            load_in(&dir.0).unwrap().pins[0]
                .processing_history
                .is_some(),
            "claim不消费历史"
        );
    }

    #[test]
    fn processing_workspace_missing_history_is_legacy_and_bad_save_preserves_assets() {
        use crate::pin_model::ProcessingHistory;
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let id = session.id.clone();
        let (raw, image, state, metadata, _) = processing_host_fixture();
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &id,
            &state,
            placement(),
            PinStatus::Closed,
            metadata.clone(),
            |p| export::save_png_pixels(image.width, image.height, &image.bgra, p),
            |p| export::save_png_pixels(raw.width, raw.height, &raw.bgra, p),
        )
        .unwrap();
        let before = load_in(&dir.0).unwrap();
        let before_bytes = encoded_manifest(&before).unwrap();
        let paths: Vec<_> = [
            before.pins[0].image_file.as_deref().unwrap(),
            before.pins[0].original_file.as_deref().unwrap(),
        ]
        .into_iter()
        .map(|name| asset_path_in(&dir.0, name).unwrap())
        .collect();
        let assets: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
        let files_before = all_files(&dir.0);
        let mut invalid =
            serde_json::to_value(metadata.processing_history.as_ref().unwrap()).unwrap();
        invalid["current"]["brightness"] = serde_json::json!(101);
        let mut bad = metadata;
        bad.processing_history = Some(serde_json::from_value(invalid).unwrap());
        let called = std::cell::Cell::new(0);
        assert!(save_raster_snapshot_with_encoders_in(
            &dir.0,
            &id,
            &state,
            placement(),
            PinStatus::Closed,
            bad,
            |_| {
                called.set(called.get() + 1);
                Ok(())
            },
            |_| {
                called.set(called.get() + 1);
                Ok(())
            }
        )
        .is_err());
        assert_eq!(called.get(), 0, "语义错误在生成临时PNG之前拒绝");
        let (_, _, _, valid_metadata, _) = processing_host_fixture();
        assert!(
            save_raster_snapshot_with_encoders_in(
                &dir.0,
                &id,
                &state,
                placement(),
                PinStatus::Closed,
                valid_metadata,
                |p| export::save_png_pixels(raw.width, raw.height, &raw.bgra, p),
                |p| export::save_png_pixels(raw.width, raw.height, &raw.bgra, p),
            )
            .is_err(),
            "实际PNG尺寸错配不能发布新清单"
        );
        assert_eq!(
            encoded_manifest(&load_in(&dir.0).unwrap()).unwrap(),
            before_bytes
        );
        for (path, bytes) in paths.iter().zip(assets) {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert_eq!(all_files(&dir.0), files_before);
        let mut wire = serde_json::to_value(WireWorkspace::from(&before)).unwrap();
        wire["pins"][0]["processingHistory"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<WireWorkspace>(wire.clone()).is_err());
        wire["pins"][0]
            .as_object_mut()
            .unwrap()
            .remove("processingHistory");
        let old = serde_json::from_value::<WireWorkspace>(wire)
            .unwrap()
            .into_model()
            .unwrap();
        assert_eq!(old.pins[0].state.adjustments, state.adjustments);
        assert!(old.pins[0].processing_history.is_none());
        write_manifest(&dir.0, &encoded_manifest(&old).unwrap()).unwrap();
        drop(session);
        let (_lease, original, entry) = Session::claim_in(&dir.0, &id).unwrap();
        assert_eq!(original.bgra, raw.bgra);
        let mut empty = ProcessingHistory::default();
        let mut current = entry.state.adjustments;
        assert!(!empty.undo(&mut current) && !empty.redo(&mut current));
        assert_eq!(current, state.adjustments);
    }

    #[test]
    fn processing_document_budget_keeps_steps_when_other_records_fill_the_library() {
        use crate::{
            annotation::{Annotation, Point, Style, Tool},
            editor::Editor,
            geometry::Rect,
            pin_model::ProcessingHistory,
        };
        let dir = TestDir::new();
        for _ in 0..MAX_CLOSED {
            drop(register(&dir.0, PinStatus::Closed));
        }
        let mut before = load_in(&dir.0).unwrap();
        before.pins[0].updated_ms = now_ms().saturating_sub(60_000);
        let oldest = before.pins[0].id.clone();
        write_manifest(&dir.0, &encoded_manifest(&before).unwrap()).unwrap();
        let session = register(&dir.0, PinStatus::Active);
        let (raw, image, state, metadata, _) = processing_host_fixture();
        save_raster_snapshot_with_encoders_in(
            &dir.0,
            &session.id,
            &state,
            placement(),
            PinStatus::Closed,
            metadata,
            |p| export::save_png_pixels(image.width, image.height, &image.bgra, p),
            |p| export::save_png_pixels(raw.width, raw.height, &raw.bgra, p),
        )
        .unwrap();
        let mut workspace = load_in(&dir.0).unwrap();
        assert_eq!(workspace.pins.len(), MAX_CLOSED);
        assert!(!workspace.pins.iter().any(|p| p.id == oldest));
        let index = workspace
            .pins
            .iter()
            .position(|p| p.id == session.id)
            .unwrap();
        let mut history = ProcessingHistory::default();
        let mut current = Adjustments::default();
        for brightness in 1..=64 {
            assert!(history.change(
                &mut current,
                Adjustments {
                    brightness,
                    ..Default::default()
                }
            ));
        }
        for _ in 0..17 {
            assert!(history.undo(&mut current));
        }
        workspace.pins[index].state.adjustments = current;
        workspace.pins[index].raster_document = Some(WireRasterDocument::empty((3, 2)).unwrap());
        workspace.pins[index].processing_history =
            Some(WireProcessingHistory::from_history(&history, current, (3, 2), (3, 2)).unwrap());
        // 总库可以超过旧清单上限，各记录的文档与处理历史独立计量。
        fn attach_other(pin: &mut PinEntry, bytes: usize) {
            let full = Rect::full(3, 2);
            let mut editor = Editor::new(full, Some(full));
            let mut a = Annotation::new(Tool::Text, Point { x: 0., y: 0. }, Style::default());
            a.update(Point { x: 3., y: 2. });
            a.text = "\u{0001}".repeat(bytes);
            editor.document.insert(a);
            if pin.image_file.is_none() {
                pin.image_file = Some(format!("{}-asset-{}.png", pin.id, unique_suffix()));
            }
            if pin.original_file.is_none() {
                pin.original_file = Some(format!("{}-asset-{}.png", pin.id, unique_suffix()));
            }
            pin.raster_document = Some(WireRasterDocument::from_editor(&editor, (3, 2)).unwrap());
        }
        attach_other(&mut workspace.pins[(index + 1) % MAX_CLOSED], 20_000);
        let other_index = (index + 2) % workspace.pins.len();
        attach_other(&mut workspace.pins[other_index], 24_000);
        let others: Vec<_> = workspace
            .pins
            .iter()
            .filter(|p| p.id != session.id)
            .map(text_host_entry_bytes)
            .collect();
        let old_doc = workspace.pins[index].raster_document.clone();
        let old = serde_json::to_value(workspace.pins[index].processing_history.as_ref().unwrap())
            .unwrap();
        assert!(encoded_manifest_bytes(&workspace).unwrap().len() as u64 > MAX_MANIFEST_BYTES);
        let mut bad = workspace.clone();
        let mut invalid = old.clone();
        invalid["current"]["turns"] = serde_json::json!(4);
        bad.pins[index].processing_history = Some(serde_json::from_value(invalid).unwrap());
        let bad_before = bad.pins[index].processing_history.clone();
        assert!(encoded_raster_manifest(&mut bad, &session.id).is_err());
        assert_eq!(
            bad.pins[index].processing_history, bad_before,
            "语义错误不能借容量裁剪"
        );
        let bytes = encoded_raster_manifest(&mut workspace, &session.id).unwrap();
        assert!(bytes.len() as u64 > MAX_MANIFEST_BYTES);
        assert!(workspace
            .pins
            .iter()
            .all(|pin| db::document_bytes(pin).unwrap() <= MAX_DOCUMENT_BYTES));
        assert_eq!(workspace.pins[index].raster_document, old_doc);
        assert_eq!(
            workspace
                .pins
                .iter()
                .filter(|p| p.id != session.id)
                .map(text_host_entry_bytes)
                .collect::<Vec<_>>(),
            others
        );
        let after =
            serde_json::to_value(workspace.pins[index].processing_history.as_ref().unwrap())
                .unwrap();
        for field in [
            "version",
            "originalWidth",
            "originalHeight",
            "width",
            "height",
            "current",
        ] {
            assert_eq!(after[field], old[field]);
        }
        assert_eq!(after["history"], old["history"]);
    }

    #[test]
    fn workspace_window_sizing_wire_is_explicit_and_legacy_missing_is_general() {
        let dir = TestDir::new();
        let (session, _) = Session::register_with_sizing_in(
            &dir.0,
            PinState::default(),
            placement(),
            PinWindowSizing::ExactCapture,
        )
        .unwrap();
        let workspace = load_in(&dir.0).unwrap();
        let mut wire = serde_json::to_value(WireWorkspace::from(&workspace)).unwrap();
        assert_eq!(wire["pins"][0]["windowSizing"], "exact-capture");
        let decoded = serde_json::from_value::<WireWorkspace>(wire.clone())
            .unwrap()
            .into_model()
            .unwrap();
        assert_eq!(decoded.pins[0].window_sizing, PinWindowSizing::ExactCapture);
        assert_eq!(decoded.pins[0].placement, workspace.pins[0].placement);
        assert_eq!(decoded.pins[0].state, workspace.pins[0].state);
        wire["pins"][0]
            .as_object_mut()
            .unwrap()
            .remove("windowSizing");
        // 缺字段不借小于24px的旧数值启发式升级为截图；普通30000px旧记录也不丢。
        for (width, height) in [(7, 11), (16_384, 899), (30_000, 24)] {
            wire["pins"][0]["placement"]["width"] = serde_json::json!(width);
            wire["pins"][0]["placement"]["height"] = serde_json::json!(height);
            let legacy = serde_json::from_value::<WireWorkspace>(wire.clone())
                .unwrap()
                .into_model()
                .unwrap();
            validate_workspace(&legacy).unwrap();
            assert_eq!(legacy.pins[0].window_sizing, PinWindowSizing::General);
            assert_eq!(
                (
                    legacy.pins[0].placement.width,
                    legacy.pins[0].placement.height
                ),
                (width, height)
            );
            assert_eq!(
                serde_json::to_value(WireWorkspace::from(&legacy)).unwrap()["pins"][0]
                    ["windowSizing"],
                "general"
            );
        }
        drop(session);
    }

    #[test]
    fn workspace_window_sizing_png_save_claim_keeps_exact_and_explicit_general() {
        let dir = TestDir::new();
        let state = PinState {
            zoom: 16_384. / 16_400.,
            ..Default::default()
        };
        let p = Placement {
            x: -5_000,
            y: -300,
            width: 16_384,
            height: 899,
        };
        let (session, entry) = Session::register_with_sizing_in(
            &dir.0,
            state.clone(),
            p,
            PinWindowSizing::ExactCapture,
        )
        .unwrap();
        let id = entry.id;
        // 与实际产品编码核心相同：政策、正文快照和状态在一个清单事务提交。
        let original = image();
        save_snapshot_with_policy_in(
            &dir.0,
            &id,
            (original.width, original.height),
            (&state, p, Some(PinWindowSizing::ExactCapture)),
            PinStatus::Closed,
            None,
            |path| export::save_png_pixels(original.width, original.height, &original.bgra, path),
        )
        .unwrap();
        assert_eq!(
            load_in(&dir.0).unwrap().pins[0].window_sizing,
            PinWindowSizing::ExactCapture
        );
        save_raster_snapshot_with_policy_in(
            &dir.0,
            &id,
            (&state, p, Some(PinWindowSizing::ExactCapture)),
            PinStatus::Closed,
            RasterSnapshotMetadata {
                original_size: (original.width, original.height),
                presentation_style: None,
                document: WireRasterDocument::empty((original.width, original.height)).unwrap(),
                processing_history: None,
            },
            |path| export::save_png_pixels(original.width, original.height, &original.bgra, path),
            |path| export::save_png_pixels(original.width, original.height, &original.bgra, path),
        )
        .unwrap();
        drop(session);
        let (lease, restored, exact) = Session::claim_in(&dir.0, &id).unwrap();
        assert_eq!(restored.bgra, original.bgra);
        assert_eq!(exact.window_sizing, PinWindowSizing::ExactCapture);
        assert_eq!(exact.placement, p);
        assert_eq!(exact.state, state);
        // 显式普通政策允许同一尺寸/zoom，不根据数值重新推断为截图。
        update_state_with_sizing_in(&dir.0, &id, &state, p, Some(PinWindowSizing::General))
            .unwrap();
        drop(lease);
        let (_lease, _, general) = Session::claim_in(&dir.0, &id).unwrap();
        assert_eq!(general.window_sizing, PinWindowSizing::General);
        assert_eq!(general.placement, exact.placement);
        assert_eq!(general.state, exact.state);
    }

    #[test]
    fn workspace_window_sizing_state_only_commit_and_failed_snapshot_are_atomic() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Active);
        let id = &session.id;
        let before = load_in(&dir.0).unwrap();
        let old_pin = &before.pins[0];
        let asset = asset_path_in(&dir.0, old_pin.image_file.as_deref().unwrap()).unwrap();
        let asset_bytes = std::fs::read(&asset).unwrap();
        update_state_with_sizing_in(
            &dir.0,
            id,
            &old_pin.state,
            old_pin.placement,
            Some(PinWindowSizing::ExactCapture),
        )
        .unwrap();
        let exact = load_in(&dir.0).unwrap();
        assert!(exact.revision > before.revision, "仅政策变化也必须提交");
        assert_eq!(exact.pins[0].window_sizing, PinWindowSizing::ExactCapture);
        assert_eq!(exact.pins[0].image_file, old_pin.image_file);
        assert_eq!(std::fs::read(&asset).unwrap(), asset_bytes);
        let committed = db::export_wire_bytes(&dir.0).unwrap();
        let files = all_files(&dir.0);
        assert!(save_snapshot_with_policy_in(
            &dir.0,
            id,
            (2, 2),
            (
                &old_pin.state,
                old_pin.placement,
                Some(PinWindowSizing::General)
            ),
            PinStatus::Closed,
            None,
            |path| {
                std::fs::write(path, b"incomplete PNG")?;
                Err("故意拒绝编码：不得单独发布窗口政策".into())
            },
        )
        .is_err());
        assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), committed);
        assert_eq!(std::fs::read(&asset).unwrap(), asset_bytes);
        assert_eq!(all_files(&dir.0), files);
        assert_eq!(
            load_in(&dir.0).unwrap().pins[0].window_sizing,
            PinWindowSizing::ExactCapture
        );
        update_state_with_sizing_in(
            &dir.0,
            id,
            &old_pin.state,
            old_pin.placement,
            Some(PinWindowSizing::General),
        )
        .unwrap();
        assert_eq!(
            load_in(&dir.0).unwrap().pins[0].window_sizing,
            PinWindowSizing::General
        );
    }

    #[test]
    fn workspace_invalid_legacy_window_sizing_is_rejected_without_rewriting_record() {
        let dir = TestDir::new();
        let session = register(&dir.0, PinStatus::Closed);
        let old = db::export_wire_bytes(&dir.0).unwrap();
        let wire: serde_json::Value = serde_json::from_slice(&old).unwrap();
        for invalid in [
            serde_json::json!("exact"),
            serde_json::Value::Null,
            serde_json::json!(true),
            serde_json::json!(3),
        ] {
            let mut value = wire.clone();
            value["pins"][0]["windowSizing"] = invalid;
            let bad = serde_json::to_vec(&value).unwrap();
            let legacy = TestDir::new();
            std::fs::write(manifest(&legacy.0), &bad).unwrap();
            assert!(load_in(&legacy.0).is_err());
            assert_eq!(std::fs::read(manifest(&legacy.0)).unwrap(), bad);
        }
        let mut value = wire.clone();
        value["pins"][0]["windowSizing"] = serde_json::json!("exact-capture");
        value["pins"][0]["placement"]["width"] = serde_json::json!(16_385);
        let bad = serde_json::to_vec(&value).unwrap();
        let legacy = TestDir::new();
        std::fs::write(manifest(&legacy.0), &bad).unwrap();
        assert!(load_in(&legacy.0).is_err());
        assert_eq!(std::fs::read(manifest(&legacy.0)).unwrap(), bad);
        assert_eq!(db::export_wire_bytes(&dir.0).unwrap(), old);
        drop(session);
        let (_lease, _, entry) =
            Session::claim_in(&dir.0, &load_in(&dir.0).unwrap().pins[0].id).unwrap();
        assert_eq!(entry.window_sizing, PinWindowSizing::General);
    }

    /// 工作区里的全部文件，相对路径用 `/` 分隔（含 pins/ 与 .runtime/）。
    fn all_files(dir: &Path) -> std::collections::BTreeSet<String> {
        fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeSet<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    walk(root, &path, out);
                } else {
                    let relative = path.strip_prefix(root).unwrap();
                    out.insert(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let mut out = std::collections::BTreeSet::new();
        walk(dir, dir, &mut out);
        out
    }
    /// pins/ 下的贴图资源文件。
    fn workspace_assets(dir: &Path) -> Vec<String> {
        all_files(dir)
            .into_iter()
            .filter(|name| name.starts_with("pins/") && name.ends_with(".png"))
            .collect()
    }
    fn root_entries(dir: &Path) -> std::collections::BTreeSet<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !matches!(name.as_str(), "library.sqlite3-wal" | "library.sqlite3-shm"))
            .collect()
    }

    #[test]
    fn pin_workspace_layout_keeps_only_images_in_flat_pins_folder() {
        let dir = TestDir::new();
        let first = register(&dir.0, PinStatus::Active);
        let second = register(&dir.0, PinStatus::Active);
        assert_eq!(
            root_entries(&dir.0),
            ["library.sqlite3", PINS_DIR, RUNTIME_DIR]
                .map(String::from)
                .into()
        );
        let workspace = load_in(&dir.0).unwrap();
        let names: Vec<_> = workspace
            .pins
            .iter()
            .map(|pin| pin.image_file.clone().unwrap())
            .collect();
        for name in &names {
            let stem = asset_stem(name).unwrap();
            assert_eq!(name, &format!("{stem}.png"));
            assert!(asset_path_in(&dir.0, name).unwrap().is_file());
        }
        assert_ne!(asset_stem(&names[0]), asset_stem(&names[1]));
        assert!(runtime_dir(&dir.0)
            .join(format!("{}.lease", first.id))
            .exists());
        // 再次保存：新快照写成 .v2，旧的提交后回收；下一次又回到本名。
        let stem = asset_stem(&names[0]).unwrap().to_string();
        let current = |dir: &Path| {
            load_in(dir)
                .unwrap()
                .pins
                .iter()
                .find(|pin| pin.id == first.id)
                .unwrap()
                .image_file
                .clone()
                .unwrap()
        };
        for expected in [format!("{stem}.v2.png"), format!("{stem}.png")] {
            save_snapshot_in(
                &dir.0,
                &first.id,
                &image(),
                &PinState::default(),
                placement(),
                PinStatus::Active,
            )
            .unwrap();
            assert_eq!(current(&dir.0), expected);
            let mine: Vec<_> = root_entries(&dir.0.join(PINS_DIR))
                .into_iter()
                .filter(|name| asset_stem(name) == Some(stem.as_str()))
                .collect();
            assert_eq!(mine, vec![expected.clone()]);
        }
        // 删除记录后它的图片一并回收，pins/ 里只剩另一张。
        second.forget().unwrap();
        assert_eq!(
            root_entries(&dir.0.join(PINS_DIR)),
            [format!("{stem}.png")].into()
        );
        drop(first);
    }

    #[test]
    fn pin_workspace_asset_names_follow_creation_time_without_spaces() {
        for stem in ["20261018-135223", "20261018-135223-2"] {
            assert!(valid_pin_stem(stem), "{stem}");
        }
        for bad in [
            "20261018-13522",
            "20261018-135223-1",
            "20261018-135223-02",
            "20261018-135223-",
            "20261018 135223",
            "2026-10-18 135223",
            "../20261018-135223",
            "20261018-135223-2x",
        ] {
            assert!(!valid_pin_stem(bad), "{bad}");
        }
        assert_eq!(
            parse_asset("20261018-135223.png"),
            Some(("20261018-135223", false, 1))
        );
        assert_eq!(
            parse_asset("20261018-135223-2.original.v3.png"),
            Some(("20261018-135223-2", true, 3))
        );
        for bad in [
            "20261018-135223.v1.png",
            "20261018-135223.v02.png",
            "20261018-135223.thumb.png",
            "20261018-135223.jpg",
            "20261018-135223/../x.png",
        ] {
            assert!(parse_asset(bad).is_none(), "{bad}");
        }
        assert!(asset_path_in(Path::new("w"), "20261018-135223/../x.png").is_err());
        assert_eq!(
            civil_from_days(days_from_civil(2026, 10, 18)),
            (2026, 10, 18)
        );
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        let base = pin_stem_base(&new_id("pin"));
        assert!(valid_pin_stem(&base), "{base}");
    }

    #[test]
    fn pin_workspace_layout_cleanup_preserves_unowned_files_and_live_assets() {
        let dir = TestDir::new();
        let runtime = ensure_runtime_dir(&dir.0).unwrap();
        let orphan_id = new_id("pin");
        let orphan_asset = format!("{orphan_id}-asset-{}.png", unique_suffix());
        let stale = [
            format!("{orphan_id}.lease"),
            format!("{orphan_asset}.tmp"),
            format!("{}.tmp", new_id("workspace")),
        ];
        let unrelated = ["notes.lease", "notes.tmp", "workspace-not-owned.tmp"];
        let live_id = new_id("pin");
        let _lease = new_lease(&dir.0, &live_id).unwrap();
        let live_temp = format!("{live_id}-asset-{}.png.tmp", unique_suffix());
        let old = SystemTime::now() - Duration::from_secs(7200);
        for folder in [&dir.0, &runtime] {
            for name in unrelated
                .iter()
                .copied()
                .chain(stale.iter().map(String::as_str))
                .chain(std::iter::once(live_temp.as_str()))
            {
                let path = folder.join(name);
                std::fs::write(&path, b"keep or reclaim").unwrap();
                OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_times(std::fs::FileTimes::new().set_modified(old))
                    .unwrap();
            }
        }
        std::fs::write(dir.0.join(&orphan_asset), b"orphan").unwrap();
        let _lock = global_lock(&dir.0).unwrap();
        cleanup_orphans(&dir.0, &Workspace::default(), None);
        for folder in [&dir.0, &runtime] {
            for name in unrelated
                .iter()
                .copied()
                .chain(std::iter::once(live_temp.as_str()))
            {
                assert_eq!(
                    std::fs::read(folder.join(name)).unwrap(),
                    b"keep or reclaim",
                    "{name}"
                );
            }
            for name in &stale {
                assert!(!folder.join(name).exists(), "{name}");
            }
        }
        assert!(!dir.0.join(orphan_asset).exists());
    }

    #[test]
    fn pin_workspace_flat_root_layout_migrates_to_pins_folder_on_first_lock() {
        let source = TestDir::new();
        let session = register(&source.0, PinStatus::Active);
        let id = session.id.clone();
        drop(session);
        // 造一个旧版工作区：资源、租约、锁都在根目录，清单 version 1。
        let mut workspace = load_in(&source.0).unwrap();
        let current = workspace.pins[0].image_file.clone().unwrap();
        let bytes = std::fs::read(asset_path_in(&source.0, &current).unwrap()).unwrap();
        let dir = TestDir::new();
        let legacy = format!("{id}-asset-{}.png", unique_suffix());
        std::fs::write(dir.0.join(&legacy), &bytes).unwrap();
        workspace.pins[0].image_file = Some(legacy.clone());
        let mut wire = serde_json::to_value(WireWorkspace::from(&workspace)).unwrap();
        wire["version"] = 1.into();
        let legacy_bytes = serde_json::to_vec(&wire).unwrap();
        std::fs::write(manifest(&dir.0), &legacy_bytes).unwrap();
        std::fs::write(dir.0.join(format!("{id}.lease")), b"").unwrap();
        std::fs::write(dir.0.join("workspace.lock"), b"").unwrap();
        // 未加锁的读取仍能按旧名找到资源。
        let loaded = load_in(&dir.0).unwrap();
        let old_name = loaded.pins[0].image_file.clone().unwrap();
        assert_eq!(
            std::fs::read(asset_path_in(&dir.0, &old_name).unwrap()).unwrap(),
            bytes
        );
        // 任一加锁事务触发迁移。
        clear_closed_in(&dir.0).unwrap();
        let migrated = load_in(&dir.0).unwrap();
        let name = migrated.pins[0].image_file.clone().unwrap();
        assert_eq!(name, format!("{}.png", pin_stem_base(&id)));
        assert_eq!(
            std::fs::read(asset_path_in(&dir.0, &name).unwrap()).unwrap(),
            bytes
        );
        assert_eq!(
            root_entries(&dir.0),
            ["library.sqlite3", "workspace.json", PINS_DIR, RUNTIME_DIR]
                .map(String::from)
                .into()
        );
        assert_eq!(root_entries(&dir.0.join(PINS_DIR)), [name.clone()].into());
        let wire: serde_json::Value =
            serde_json::from_slice(&db::export_wire_bytes(&dir.0).unwrap()).unwrap();
        assert_eq!(wire["version"], LAYOUT_VERSION);
        assert_eq!(std::fs::read(manifest(&dir.0)).unwrap(), legacy_bytes);
        let (_session, image, _) = Session::claim_in(&dir.0, &id).unwrap();
        assert_eq!((image.width, image.height), (2, 2));
    }
}
