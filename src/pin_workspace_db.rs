//! 贴图管理行与矢量文档分表保存；仅访问存储时打开数据库。
use super::*;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

const SCHEMA_VERSION: i64 = 1;

#[derive(Default, Serialize, Deserialize)]
struct Documents {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<WireTextDocument>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    raster: Option<WireRasterDocument>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    processing: Option<WireProcessingHistory>,
}

pub(super) fn document_bytes(pin: &PinEntry) -> Result<usize> {
    Ok(serde_json::to_vec(&Documents {
        text: pin.text_document.clone(),
        raster: pin.raster_document.clone(),
        processing: pin.processing_history.clone(),
    })?
    .len())
}

fn initialized(connection: &Connection) -> Result<bool> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='pin_meta')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    let version: Option<i64> = connection
        .query_row(
            "SELECT schema_version FROM pin_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match version {
        None => Ok(false),
        Some(SCHEMA_VERSION) => Ok(true),
        Some(_) => Err("不支持的贴图数据库版本，已保留原数据库".into()),
    }
}

fn open(dir: &Path) -> Result<Connection> {
    let mut connection = crate::storage_db::open_workspace(dir)?;
    if initialized(&connection)? {
        return Ok(connection);
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS pin_meta (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            schema_version INTEGER NOT NULL,
            layout_version INTEGER NOT NULL,
            current_group TEXT NOT NULL,
            restore_on_startup INTEGER NOT NULL,
            revision TEXT NOT NULL,
            legacy_imported INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS pin_groups (
            id TEXT PRIMARY KEY,
            position INTEGER NOT NULL,
            name TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS pin_items (
            id TEXT PRIMARY KEY,
            position INTEGER NOT NULL,
            group_id TEXT NOT NULL REFERENCES pin_groups(id),
            status TEXT NOT NULL,
            metadata_json TEXT NOT NULL,
            has_text INTEGER NOT NULL,
            has_raster INTEGER NOT NULL,
            preview_text TEXT
         );
         CREATE INDEX IF NOT EXISTS pin_items_group ON pin_items(group_id, position);
         CREATE INDEX IF NOT EXISTS pin_items_status ON pin_items(status);
         CREATE TABLE IF NOT EXISTS pin_documents (
            pin_id TEXT PRIMARY KEY REFERENCES pin_items(id) ON DELETE CASCADE,
            document_json TEXT NOT NULL
         );",
    )?;
    if !initialized(&transaction)? {
        // 只有这次原子导入读取旧 JSON。成功后原文件保留作备份，后续访问只信数据库。
        let workspace = load_legacy_in(dir)?;
        persist(&transaction, WireWorkspace::from(&workspace))?;
    }
    transaction.commit()?;
    Ok(connection)
}

pub(super) fn load(dir: &Path) -> Result<Workspace> {
    let mut connection = open(dir)?;
    let transaction = connection.transaction()?;
    let wire = read_wire(&transaction, true)?;
    let workspace = wire.into_model()?;
    validate_workspace(&workspace)?;
    transaction.commit()?;
    Ok(workspace)
}

fn read_header(connection: &Connection) -> Result<WireWorkspace> {
    let (version, current_group, restore, revision): (u32, String, bool, String) = connection
        .query_row(
            "SELECT layout_version, current_group, restore_on_startup, revision
             FROM pin_meta WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    let groups = connection
        .prepare("SELECT id, name FROM pin_groups ORDER BY position")?
        .query_map([], |row| {
            Ok(WireGroup {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(WireWorkspace {
        version,
        groups,
        current_group,
        restore_on_startup: restore,
        pins: Vec::new(),
        revision: revision.parse()?,
    })
}

fn read_wire(connection: &Connection, documents: bool) -> Result<WireWorkspace> {
    let mut wire = read_header(connection)?;
    let sql = if documents {
        "SELECT p.id, p.group_id, p.status, p.metadata_json, d.document_json
         FROM pin_items p LEFT JOIN pin_documents d ON d.pin_id=p.id ORDER BY p.position"
    } else {
        "SELECT id, group_id, status, metadata_json, NULL FROM pin_items ORDER BY position"
    };
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    for row in rows {
        let (id, group, status, metadata, document) = row?;
        let mut pin: WirePin = serde_json::from_str(&metadata)?;
        if pin.id != id || pin.group_id != group || pin.status != status {
            return Err("贴图数据库管理字段不一致".into());
        }
        if let Some(document) = document {
            if document.len() > MAX_DOCUMENT_BYTES {
                return Err("单张贴图文档超过大小限制".into());
            }
            let document: Documents = serde_json::from_str(&document)?;
            pin.text_document = document.text;
            pin.raster_document = document.raster;
            pin.processing_history = document.processing;
        }
        wire.pins.push(pin);
    }
    Ok(wire)
}

pub(super) fn write(dir: &Path, bytes: &[u8]) -> Result<()> {
    let wire: WireWorkspace = serde_json::from_slice(bytes)?;
    let workspace = wire.into_model()?;
    // 保留现有语义及每张文档预算；数据库不再让不同贴图争用旧清单字节上限。
    encoded_manifest(&workspace)?;
    let mut connection = open(dir)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    persist(&transaction, WireWorkspace::from(&workspace))?;
    transaction.commit()?;
    Ok(())
}

fn persist(transaction: &Transaction<'_>, wire: WireWorkspace) -> Result<()> {
    for (position, group) in wire.groups.iter().enumerate() {
        transaction.execute(
            "INSERT INTO pin_groups(id, position, name) VALUES(?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET position=excluded.position, name=excluded.name
             WHERE position IS NOT excluded.position OR name IS NOT excluded.name",
            params![group.id, position as i64, group.name],
        )?;
    }
    let current: HashSet<&str> = wire.pins.iter().map(|pin| pin.id.as_str()).collect();
    let previous = transaction
        .prepare("SELECT id FROM pin_items")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for id in previous {
        if !current.contains(id.as_str()) {
            transaction.execute("DELETE FROM pin_items WHERE id=?1", [&id])?;
        }
    }
    for (position, mut pin) in wire.pins.into_iter().enumerate() {
        let has_text = pin.text_document.is_some();
        let has_raster = pin.raster_document.is_some();
        let preview = pin.text_document.as_ref().and_then(preview_text);
        let documents = Documents {
            text: pin.text_document.take(),
            raster: pin.raster_document.take(),
            processing: pin.processing_history.take(),
        };
        let document_json = serde_json::to_string(&documents)?;
        if document_json.len() > MAX_DOCUMENT_BYTES {
            return Err("单张贴图文档超过大小限制".into());
        }
        let metadata = serde_json::to_string(&pin)?;
        transaction.execute(
            "INSERT INTO pin_items(id, position, group_id, status, metadata_json, has_text, has_raster, preview_text)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET position=excluded.position, group_id=excluded.group_id,
               status=excluded.status, metadata_json=excluded.metadata_json,
               has_text=excluded.has_text, has_raster=excluded.has_raster, preview_text=excluded.preview_text
             WHERE position IS NOT excluded.position OR group_id IS NOT excluded.group_id
               OR status IS NOT excluded.status OR metadata_json IS NOT excluded.metadata_json
               OR has_text IS NOT excluded.has_text OR has_raster IS NOT excluded.has_raster
               OR preview_text IS NOT excluded.preview_text",
            params![pin.id, position as i64, pin.group_id, pin.status, metadata, has_text, has_raster, preview],
        )?;
        transaction.execute(
            "INSERT INTO pin_documents(pin_id, document_json) VALUES(?1, ?2)
             ON CONFLICT(pin_id) DO UPDATE SET document_json=excluded.document_json
             WHERE document_json IS NOT excluded.document_json",
            params![pin.id, document_json],
        )?;
    }
    let current_groups: HashSet<&str> = wire.groups.iter().map(|group| group.id.as_str()).collect();
    let previous = transaction
        .prepare("SELECT id FROM pin_groups")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for id in previous {
        if !current_groups.contains(id.as_str()) {
            transaction.execute("DELETE FROM pin_groups WHERE id=?1", [&id])?;
        }
    }
    transaction.execute(
        "INSERT INTO pin_meta(singleton,schema_version,layout_version,current_group,restore_on_startup,revision,legacy_imported)
         VALUES(1,?1,?2,?3,?4,?5,1)
         ON CONFLICT(singleton) DO UPDATE SET layout_version=excluded.layout_version,
           current_group=excluded.current_group, restore_on_startup=excluded.restore_on_startup,
           revision=excluded.revision
         WHERE layout_version IS NOT excluded.layout_version OR current_group IS NOT excluded.current_group
           OR restore_on_startup IS NOT excluded.restore_on_startup OR revision IS NOT excluded.revision",
        params![SCHEMA_VERSION, wire.version, wire.current_group, wire.restore_on_startup, wire.revision.to_string()],
    )?;
    Ok(())
}

fn preview_text(document: &WireTextDocument) -> Option<String> {
    let mut preview = String::new();
    let mut remaining = 240;
    for text in document
        .current_texts()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        if !preview.is_empty() {
            preview.push('\n');
            remaining -= 1;
        }
        preview.extend(text.chars().take(remaining));
        remaining = 240 - preview.chars().count();
        if remaining == 0 {
            break;
        }
    }
    (!preview.is_empty()).then_some(preview)
}

pub(super) fn load_summary(dir: &Path) -> Result<WorkspaceSummary> {
    let mut connection = open(dir)?;
    let transaction = connection.transaction()?;
    let summary = read_summary(&transaction)?;
    transaction.commit()?;
    Ok(summary)
}

fn read_summary(connection: &Connection) -> Result<WorkspaceSummary> {
    let workspace = read_wire(connection, false)?.into_model()?;
    let mut infos =
        connection.prepare("SELECT id, has_text, has_raster, preview_text FROM pin_items")?;
    let info = infos
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ),
            ))
        })?
        .collect::<std::result::Result<std::collections::HashMap<_, _>, _>>()?;
    drop(infos);
    let mut pins = Vec::with_capacity(workspace.pins.len());
    for pin in workspace.pins {
        validate_pin_id(&pin.id)?;
        validate_state(&pin.state, pin.placement)?;
        validate_window_sizing(pin.window_sizing, pin.placement)?;
        let (text, has_raster_document, preview_text) =
            info.get(&pin.id).cloned().ok_or("贴图摘要缺少来源字段")?;
        pins.push(PinSummary {
            id: pin.id,
            group_id: pin.group_id,
            title: pin.title,
            status: pin.status,
            hidden: pin.hidden,
            click_through: pin.click_through,
            placement: pin.placement,
            window_sizing: pin.window_sizing,
            state: pin.state,
            image_file: pin.image_file,
            original_file: pin.original_file,
            presentation_style: pin.presentation_style,
            updated_ms: pin.updated_ms,
            text,
            preview_text,
            has_raster_document,
        });
    }
    Ok(WorkspaceSummary {
        groups: workspace.groups,
        current_group: workspace.current_group,
        restore_on_startup: workspace.restore_on_startup,
        revision: workspace.revision,
        pins,
    })
}

/// 只观察已经导入完成的主库；空目录和目录索引先建库均不触发初始化。
pub(super) fn existing_schema(dir: &Path) -> Result<bool> {
    let Some(connection) = crate::storage_db::open_workspace_readonly(dir)? else {
        return Ok(false);
    };
    initialized(&connection)
}

pub(super) fn relocation_summary(dir: &Path, root_name: &str) -> Result<Option<WorkspaceSummary>> {
    let Some(mut connection) = crate::storage_db::open_workspace_readonly(dir)? else {
        return Ok(None);
    };
    if !initialized(&connection)? {
        return Ok(None);
    }
    let transaction = connection.transaction()?;
    let has_table: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='pin_asset_relocations')",
        [], |row| row.get(0),
    )?;
    if has_table
        && transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM pin_asset_relocations WHERE root_name=?1)",
            [root_name],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Ok(None);
    }
    let summary = read_summary(&transaction)?;
    transaction.commit()?;
    Ok(Some(summary))
}

pub(super) fn finish_relocation(dir: &Path, root_name: &str) -> Result<()> {
    // 只读句柄保持打开，避免此流程把消失的主库重建成空库。
    let existing =
        crate::storage_db::open_workspace_readonly(dir)?.ok_or("贴图库在资源补齐期间消失")?;
    if !initialized(&existing)? {
        return Err("贴图库尚未完成导入，资源补齐未确认".into());
    }
    let mut connection = crate::storage_db::open_workspace(dir)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS pin_asset_relocations(root_name TEXT PRIMARY KEY);",
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO pin_asset_relocations(root_name) VALUES(?1)",
        [root_name],
    )?;
    transaction.commit()?;
    drop(existing);
    Ok(())
}

fn metadata_pin(connection: &Connection, id: &str) -> Result<WirePin> {
    let json: String = connection
        .query_row(
            "SELECT metadata_json FROM pin_items WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or("贴图记录不存在")?;
    let pin: WirePin = serde_json::from_str(&json)?;
    if pin.id != id {
        return Err("贴图数据库记录标识不一致".into());
    }
    Ok(pin)
}

fn update_metadata(transaction: &Transaction<'_>, pin: &WirePin) -> Result<()> {
    transaction.execute(
        "UPDATE pin_items SET metadata_json=?1 WHERE id=?2",
        params![serde_json::to_string(pin)?, pin.id],
    )?;
    increment_revision(transaction)
}

fn increment_revision(transaction: &Transaction<'_>) -> Result<()> {
    let revision: String = transaction.query_row(
        "SELECT revision FROM pin_meta WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let revision = revision
        .parse::<u64>()?
        .checked_add(1)
        .ok_or("工作区版本计数溢出")?;
    transaction.execute(
        "UPDATE pin_meta SET revision=?1 WHERE singleton=1",
        [revision.to_string()],
    )?;
    Ok(())
}

pub(super) fn remove_records(dir: &Path, ids: &[String]) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut connection = open(dir)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut removed = 0;
    for id in ids {
        removed += transaction.execute("DELETE FROM pin_items WHERE id=?1", [id])?;
    }
    if removed > 0 {
        increment_revision(&transaction)?;
    }
    transaction.commit()?;
    Ok(removed)
}

pub(super) fn persist_interaction(
    dir: &Path,
    id: &str,
    locked: bool,
    click_through: bool,
) -> Result<()> {
    let mut connection = open(dir)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut pin = metadata_pin(&transaction, id)?;
    if pin.status != "active" {
        return Err("贴图活动记录不存在，交互状态未保存".into());
    }
    if pin.state.locked != locked || pin.click_through != click_through {
        pin.state.locked = locked;
        pin.click_through = click_through;
        update_metadata(&transaction, &pin)?;
    }
    transaction.commit()?;
    Ok(())
}

/// 返回 false 表示处理参数变化，调用方沿完整文档路径重新验证关联尺寸与历史。
pub(super) fn update_state(
    dir: &Path,
    id: &str,
    state: &PinState,
    placement: Placement,
    sizing: Option<PinWindowSizing>,
) -> Result<bool> {
    let mut connection = open(dir)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut pin = metadata_pin(&transaction, id)?;
    let next = WireState::from(state);
    if pin.state.turns != next.turns
        || pin.state.flip_x != next.flip_x
        || pin.state.flip_y != next.flip_y
        || pin.state.grayscale != next.grayscale
        || pin.state.invert != next.invert
        || pin.state.brightness != next.brightness
    {
        return Ok(false);
    }
    validate_window_sizing(
        sizing.unwrap_or_else(|| pin.window_sizing.into()),
        placement,
    )?;
    let next_placement = WirePlacement {
        x: placement.x,
        y: placement.y,
        width: placement.width,
        height: placement.height,
    };
    let next_sizing = sizing.map(Into::into).unwrap_or(pin.window_sizing);
    if pin.state != next || pin.placement != next_placement || pin.window_sizing != next_sizing {
        pin.state = next;
        pin.placement = next_placement;
        pin.window_sizing = next_sizing;
        pin.updated_ms = now_ms();
        update_metadata(&transaction, &pin)?;
    }
    transaction.commit()?;
    Ok(true)
}

pub(super) fn startup_restore_needed(dir: &Path) -> Result<bool> {
    if let Some(connection) = crate::storage_db::open_workspace_readonly(dir)? {
        if initialized(&connection)? {
            let restore: bool = connection.query_row(
                "SELECT restore_on_startup FROM pin_meta WHERE singleton=1",
                [],
                |row| row.get(0),
            )?;
            if !restore {
                return Ok(false);
            }
            let mut statement = connection.prepare(
                "SELECT metadata_json FROM pin_items WHERE status='active'
                 AND group_id=(SELECT current_group FROM pin_meta WHERE singleton=1)",
            )?;
            for row in statement.query_map([], |row| row.get::<_, String>(0))? {
                let pin: WirePin = serde_json::from_str(&row?)?;
                if startup_pin_needed(dir, &pin) {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
    }
    let workspace = load_legacy_in(dir)?;
    if !workspace.restore_on_startup {
        return Ok(false);
    }
    Ok(WireWorkspace::from(&workspace)
        .pins
        .iter()
        .any(|pin| pin.group_id == workspace.current_group && startup_pin_needed(dir, pin)))
}

fn startup_pin_needed(dir: &Path, pin: &WirePin) -> bool {
    if pin.status != "active"
        || pin.hidden
        || pin.image_file.is_none()
        || lease_is_live(dir, &pin.id)
    {
        return false;
    }
    [&pin.image_file, &pin.original_file]
        .into_iter()
        .flatten()
        .all(|name| {
            asset_path_in(dir, name)
                .is_ok_and(|path| crate::image_collection::ensure_local_file(&path).is_ok())
        })
}

#[cfg(test)]
pub(super) fn export_wire_bytes(dir: &Path) -> Result<Vec<u8>> {
    let connection = open(dir)?;
    Ok(serde_json::to_vec(&read_wire(&connection, true)?)?)
}

#[cfg(test)]
#[path = "pin_workspace_db_tests.rs"]
mod tests;
