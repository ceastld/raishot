//! 统一历史的可重建元数据索引；图片和贴图恢复状态仍由各自来源持有。
use super::{Item, Source};
use crate::{
    file_snapshot,
    recording_library_metadata::{self, Probe, VideoInfo},
    Result,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::HashMap,
    io,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) struct Catalog {
    connection: Connection,
}

impl Catalog {
    /// 仅在历史扫描中打开；托盘与截图启动不创建库或进行迁移。
    pub(super) fn open() -> Result<Self> {
        Self::open_in(&crate::preferences::data_dir().join("pin-workspace"))
    }

    fn open_in(directory: &Path) -> Result<Self> {
        let connection = crate::storage_db::open_workspace(directory)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS catalog_items (
                item_key TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                created_secs INTEGER NOT NULL,
                created_nanos INTEGER NOT NULL,
                source_title TEXT NOT NULL,
                title TEXT,
                width INTEGER,
                height INTEGER,
                bytes INTEGER,
                source_path TEXT,
                source_bytes INTEGER,
                modified_secs INTEGER,
                modified_nanos INTEGER,
                duration_secs INTEGER,
                duration_nanos INTEGER
            );
            CREATE INDEX IF NOT EXISTS catalog_items_time
                ON catalog_items(created_secs DESC, created_nanos DESC, item_key);
            CREATE INDEX IF NOT EXISTS catalog_items_kind_time
                ON catalog_items(kind, created_secs DESC, created_nanos DESC, item_key);
            CREATE TABLE IF NOT EXISTS catalog_recording_metadata (
                source_path TEXT PRIMARY KEY,
                snapshot BLOB NOT NULL,
                width INTEGER,
                height INTEGER,
                duration_secs INTEGER,
                duration_nanos INTEGER
            );",
        )?;
        Ok(Self { connection })
    }

    pub(super) fn probe_recording(
        &self,
        path: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Probe> {
        self.probe_recording_with(path, cancelled, recording_library_metadata::probe)
    }

    fn probe_recording_with(
        &self,
        path: &Path,
        cancelled: &dyn Fn() -> bool,
        probe: impl FnOnce(&Path, &dyn Fn() -> bool) -> io::Result<Probe>,
    ) -> io::Result<Probe> {
        if cancelled() {
            return Ok(Probe::Cancelled);
        }
        // 元数据命中也不为离线文件、目录链接或云端占位授予可读取资格。
        if crate::image_collection::ensure_local_file(path).is_err() {
            return Ok(Probe::Unavailable);
        }
        let before = fingerprint(path);
        if let Some(snapshot) = &before {
            let cached = self
                .connection
                .query_row(
                    "SELECT width, height, duration_secs, duration_nanos
                 FROM catalog_recording_metadata WHERE source_path = ?1 AND snapshot = ?2",
                    params![path.to_string_lossy(), snapshot],
                    video_info,
                )
                .optional();
            if let Ok(Some(info)) = cached {
                if cancelled() {
                    return Ok(Probe::Cancelled);
                }
                if fingerprint(path).as_ref() == Some(snapshot) {
                    return Ok(Probe::Complete(info));
                }
            }
        }
        let result = probe(path, cancelled)?;
        if cancelled() {
            return Ok(Probe::Cancelled);
        }
        if let (Some(snapshot), Probe::Complete(info)) = (before, result) {
            // 在探测前后核对原生文件身份，不能把旧容器结果绑定到替换后的同名文件。
            if fingerprint(path).as_ref() != Some(&snapshot) {
                return Ok(Probe::Unavailable);
            }
            if let Ok(duration) = duration_parts(info.duration) {
                let _ = self.connection.execute(
                    "INSERT INTO catalog_recording_metadata
                         (source_path, snapshot, width, height, duration_secs, duration_nanos)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                         ON CONFLICT(source_path) DO UPDATE SET snapshot = excluded.snapshot,
                            width = excluded.width, height = excluded.height,
                            duration_secs = excluded.duration_secs,
                            duration_nanos = excluded.duration_nanos",
                    params![
                        path.to_string_lossy(),
                        snapshot,
                        info.width,
                        info.height,
                        duration.0,
                        duration.1
                    ],
                );
            }
        }
        Ok(result)
    }

    /// 来源失败时保留其索引；当前列表只消费本轮取得的动作来源，绝不恢复失效状态。
    pub(super) fn reconcile(
        &mut self,
        items: &[Item],
        complete: [bool; 3],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<Item>> {
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS catalog_seen (item_key TEXT PRIMARY KEY);
             DELETE FROM catalog_seen;",
        )?;
        for item in items {
            if cancelled() {
                return Err("历史索引更新已取消".into());
            }
            let (kind, path, source_bytes, modified) = source_fields(&item.source);
            let created = time_parts(item.created)?;
            let modified = modified.map(time_parts).transpose()?;
            let duration = duration_parts(item.duration())?;
            let (width, height) = item.size.unzip();
            transaction.execute(
                "INSERT INTO catalog_items
                 (item_key, kind, created_secs, created_nanos, source_title,
                  width, height, bytes, source_path, source_bytes, modified_secs,
                  modified_nanos, duration_secs, duration_nanos)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT(item_key) DO UPDATE SET kind = excluded.kind,
                    created_secs = excluded.created_secs, created_nanos = excluded.created_nanos,
                    source_title = excluded.source_title, width = excluded.width,
                    height = excluded.height, bytes = excluded.bytes,
                    source_path = excluded.source_path, source_bytes = excluded.source_bytes,
                    modified_secs = excluded.modified_secs, modified_nanos = excluded.modified_nanos,
                    duration_secs = excluded.duration_secs, duration_nanos = excluded.duration_nanos
                 WHERE kind IS NOT excluded.kind OR created_secs IS NOT excluded.created_secs
                    OR created_nanos IS NOT excluded.created_nanos
                    OR source_title IS NOT excluded.source_title OR width IS NOT excluded.width
                    OR height IS NOT excluded.height OR bytes IS NOT excluded.bytes
                    OR source_path IS NOT excluded.source_path OR source_bytes IS NOT excluded.source_bytes
                    OR modified_secs IS NOT excluded.modified_secs OR modified_nanos IS NOT excluded.modified_nanos
                    OR duration_secs IS NOT excluded.duration_secs OR duration_nanos IS NOT excluded.duration_nanos",
                params![item.key, kind, created.0, created.1, item.title, width, height,
                    item.bytes, path, source_bytes, modified.map(|value| value.0),
                    modified.map(|value| value.1), duration.0, duration.1],
            )?;
            transaction.execute(
                "INSERT INTO catalog_seen(item_key) VALUES (?1)",
                [&item.key],
            )?;
        }
        for (kind, complete) in ["shot", "pin", "recording"].into_iter().zip(complete) {
            if complete {
                if kind == "recording" {
                    // 改名、范围切换或扫描未纳入不代表成片已删除；只清理明确缺失的路径。
                    let mut statement = transaction.prepare(
                        "SELECT item_key, source_path FROM catalog_items WHERE kind='recording'
                         AND item_key NOT IN (SELECT item_key FROM catalog_seen)",
                    )?;
                    let missing = statement
                        .query_map([], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    drop(statement);
                    for (key, path) in missing {
                        if cancelled() {
                            return Err("历史索引更新已取消".into());
                        }
                        if path.as_deref().is_some_and(recording_is_missing) {
                            transaction
                                .execute("DELETE FROM catalog_items WHERE item_key=?1", [key])?;
                        }
                    }
                } else {
                    transaction.execute(
                        "DELETE FROM catalog_items WHERE kind = ?1
                         AND item_key NOT IN (SELECT item_key FROM catalog_seen)",
                        [kind],
                    )?;
                }
            }
        }
        if complete[2] {
            transaction.execute(
                "DELETE FROM catalog_recording_metadata WHERE source_path NOT IN
                 (SELECT source_path FROM catalog_items WHERE kind = 'recording' AND source_path IS NOT NULL)",
                [],
            )?;
        }
        if cancelled() {
            return Err("历史索引更新已取消".into());
        }
        transaction.commit()?;
        let mut sources: HashMap<_, _> = items
            .iter()
            .cloned()
            .map(|item| (item.key.clone(), item))
            .collect();
        let mut statement = self.connection.prepare(
            "SELECT item_key, created_secs, created_nanos, COALESCE(title, source_title), width, height, bytes
             FROM catalog_items ORDER BY created_secs DESC, created_nanos DESC, item_key ASC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<u32>>(4)?,
                row.get::<_, Option<u32>>(5)?,
                row.get::<_, Option<u64>>(6)?,
            ))
        })?;
        let mut result = Vec::with_capacity(sources.len());
        for row in rows {
            let (key, seconds, nanos, title, width, height, bytes) = row?;
            if let Some(mut item) = sources.remove(&key) {
                item.created = time_from_parts(seconds, nanos).ok_or("历史索引时间超出范围")?;
                // 贴图标题仍以本轮工作区为准；库不会创建第二份可写贴图状态。
                if !matches!(item.source, Source::Pin { .. }) {
                    item.title = title;
                }
                item.size = width.zip(height);
                item.bytes = bytes;
                result.push(item);
            }
        }
        Ok(result)
    }
}

fn recording_is_missing(path: &str) -> bool {
    let path = Path::new(path);
    crate::image_collection::ensure_local_file_or_missing(path).is_ok()
        && std::fs::symlink_metadata(path)
            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn source_fields(
    source: &Source,
) -> (
    &'static str,
    Option<String>,
    Option<u64>,
    Option<SystemTime>,
) {
    match source {
        Source::Shot(entry) => (
            "shot",
            Some(entry.path.to_string_lossy().into()),
            Some(entry.bytes),
            Some(entry.modified),
        ),
        Source::Recording(entry) => (
            "recording",
            Some(entry.path.to_string_lossy().into()),
            Some(entry.bytes),
            entry.modified,
        ),
        Source::Pin { image, .. } => {
            let path = image
                .as_deref()
                .and_then(|name| crate::pin_workspace::asset_path(name).ok());
            let metadata = path
                .as_ref()
                .and_then(|path| std::fs::symlink_metadata(path).ok());
            (
                "pin",
                path.map(|path| path.to_string_lossy().into_owned()),
                metadata.as_ref().map(|value| value.len()),
                metadata.and_then(|value| value.modified().ok()),
            )
        }
    }
}

fn fingerprint(path: &Path) -> Option<Vec<u8>> {
    let snapshot = file_snapshot::inspect(path).ok()??;
    let mut bytes = Vec::with_capacity(52);
    bytes.extend(snapshot.volume.to_le_bytes());
    bytes.extend(snapshot.id);
    bytes.extend(snapshot.created.to_le_bytes());
    bytes.extend(snapshot.written.to_le_bytes());
    bytes.extend(snapshot.bytes.to_le_bytes());
    bytes.extend(snapshot.attributes.to_le_bytes());
    Some(bytes)
}

fn video_info(row: &rusqlite::Row<'_>) -> rusqlite::Result<VideoInfo> {
    let seconds: Option<u64> = row.get(2)?;
    let nanos: Option<u32> = row.get(3)?;
    if seconds.is_some() != nanos.is_some() || nanos.is_some_and(|value| value >= 1_000_000_000) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(VideoInfo {
        width: row.get(0)?,
        height: row.get(1)?,
        duration: seconds
            .zip(nanos)
            .map(|(seconds, nanos)| Duration::new(seconds, nanos)),
    })
}

fn duration_parts(duration: Option<Duration>) -> Result<(Option<i64>, Option<u32>)> {
    match duration {
        Some(duration) => Ok((
            Some(i64::try_from(duration.as_secs())?),
            Some(duration.subsec_nanos()),
        )),
        None => Ok((None, None)),
    }
}

fn time_parts(time: SystemTime) -> Result<(i64, u32)> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => Ok((i64::try_from(duration.as_secs())?, duration.subsec_nanos())),
        Err(error) => {
            let duration = error.duration();
            let nanos = duration.subsec_nanos();
            Ok((
                -i64::try_from(duration.as_secs())? - i64::from(nanos != 0),
                if nanos == 0 { 0 } else { 1_000_000_000 - nanos },
            ))
        }
    }
}

fn time_from_parts(seconds: i64, nanos: u32) -> Option<SystemTime> {
    if nanos >= 1_000_000_000 {
        return None;
    }
    if seconds >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(seconds as u64, nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(seconds.unsigned_abs()))?
            .checked_add(Duration::from_nanos(u64::from(nanos)))
    }
}

#[cfg(test)]
#[path = "library_catalog_tests.rs"]
mod tests;
