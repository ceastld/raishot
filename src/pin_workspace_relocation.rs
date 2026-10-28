//! 主库已迁移但图片仍留在曾用品牌目录时，按已登记资源名作一次非破坏补齐。
use super::*;
use std::io::{self, Seek, SeekFrom};
use std::os::windows::fs::MetadataExt;
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};

const UNAVAILABLE: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;

fn same_path(left: &Path, right: &Path) -> bool {
    let normalized = |path: &Path| {
        std::path::absolute(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .replace('/', "\\")
            .trim_start_matches("\\\\?\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    normalized(left) == normalized(right)
}

fn candidates_in(dir: &Path, base: &Path, custom: bool) -> Option<Vec<PathBuf>> {
    if custom
        || !same_path(
            dir,
            &base.join(crate::brand::DATA_DIR).join("pin-workspace"),
        )
    {
        return None;
    }
    Some(
        crate::brand::LEGACY_NAMES
            .iter()
            .rev()
            .map(|name| base.join(name).join("pin-workspace"))
            .filter(|candidate| !same_path(candidate, dir))
            .collect(),
    )
}

fn production_candidates(dir: &Path) -> Option<Vec<PathBuf>> {
    #[cfg(test)]
    if let Some(candidates) = tests::candidates_for(dir) {
        return Some(candidates);
    }
    let custom = std::env::var_os("RUSTSHOT_DATA_DIR").is_some();
    let base = std::env::var_os("LOCALAPPDATA")?;
    candidates_in(dir, Path::new(&base), custom)
}

pub(super) fn prepare(dir: &Path) -> Result<usize> {
    if !db::existing_schema(dir)? && !meaningful_legacy(&load_legacy_in(dir)?) {
        return Ok(0);
    }
    let _lock = global_lock_with_layout(dir, false)?;
    // 等锁期间另一进程可能已经导入；未导入时先验证，避免坏JSON创建空数据库。
    if !db::existing_schema(dir)? && !meaningful_legacy(&load_legacy_in(dir)?) {
        return Ok(0);
    }
    let summary = db::load_summary(dir)?;
    reconcile_locked(dir)?;
    Ok(summary.pins.len())
}

fn meaningful_legacy(workspace: &Workspace) -> bool {
    !workspace.pins.is_empty()
        || workspace.groups.len() > 1
        || workspace.revision > 0
        || !workspace.restore_on_startup
}

pub(super) fn reconcile(dir: &Path) -> Result<()> {
    if production_candidates(dir).is_none()
        || db::relocation_summary(dir, crate::brand::DATA_DIR)?.is_none()
    {
        return Ok(());
    }
    // 已有事务直接调用锁内入口，避免递归取得同一把锁。
    let _lock = global_lock_with_layout(dir, false)?;
    reconcile_locked(dir)
}

/// 来源暂不可读时，只暂停自动修剪和资源回收，不封锁无关的安全写入。
pub(super) fn reclaim_allowed(dir: &Path) -> bool {
    match reconcile_locked(dir) {
        Ok(()) => true,
        Err(error) => {
            eprintln!("Pin workspace asset reconciliation deferred: {error}");
            false
        }
    }
}

pub(super) fn reconcile_locked(dir: &Path) -> Result<()> {
    let Some(candidates) = production_candidates(dir) else {
        return Ok(());
    };
    reconcile_with(dir, &candidates, crate::brand::DATA_DIR)
}

/// 调用方持有主工作区事务锁；测试显式注入临时目录，永不探查本机品牌数据。
fn reconcile_with(dir: &Path, candidates: &[PathBuf], root_name: &str) -> Result<()> {
    let Some(summary) = db::relocation_summary(dir, root_name)? else {
        return Ok(());
    };
    let mut seen = HashSet::new();
    let mut plans = Vec::new();
    let mut bytes = 0_u64;
    for pin in &summary.pins {
        for name in [&pin.image_file, &pin.original_file].into_iter().flatten() {
            if !seen.insert(name.as_str()) {
                continue;
            }
            let target = asset_path_in(dir, name)?;
            if let Some(length) = available_file(&target)? {
                bytes = bytes.checked_add(length).ok_or("贴图资源容量统计溢出")?;
                continue;
            }
            let mut selected: Option<Source> = None;
            for candidate in candidates {
                let source = asset_path_in(candidate, name)?;
                let Some(source) = Source::open(&source)? else {
                    continue;
                };
                if let Some(previous) = &mut selected {
                    if !same_content(previous, source)? {
                        return Err(trf!(
                            "曾用品牌目录中存在不同的同名贴图资源：{name}",
                            "Conflicting pin assets in previous data folders: {name}"
                        )
                        .into());
                    }
                } else {
                    selected = Some(source);
                }
            }
            if let Some(source) = selected {
                bytes = bytes
                    .checked_add(source.bytes)
                    .ok_or("贴图资源容量统计溢出")?;
                plans.push((pin.id.clone(), target, source));
            }
        }
    }
    if !plans.is_empty() && bytes >= MAX_BYTES {
        return Err("补齐贴图资源会超过工作区容量限制".into());
    }
    for (id, target, mut source) in plans {
        let parent = target.parent().ok_or("贴图资源没有父目录")?;
        crate::image_collection::ensure_local_file_or_missing(&target)?;
        std::fs::create_dir_all(parent)?;
        crate::image_collection::ensure_local_directory(parent)?;
        let _target_directory = directory_guard(parent)?;
        let expected = source.bytes;
        let (mut pending, encoded_len) = PendingAsset::encode_in(dir, &id, |path| {
            let mut output = OpenOptions::new().write(true).truncate(true).open(path)?;
            source.file.seek(SeekFrom::Start(0))?;
            let copied = io::copy(&mut (&mut source.file).take(expected + 1), &mut output)?;
            if copied != expected {
                return Err("旧贴图资源在补齐期间改变，未发布副本".into());
            }
            Ok(())
        })?;
        if encoded_len != expected {
            return Err("补齐贴图资源的长度校验失败".into());
        }
        pending.path = Some(target);
        move_new(
            &pending.temp,
            pending.path.as_deref().ok_or("补齐资源缺少目标路径")?,
        )?;
        pending.renamed = true;
        // 文件名本来就在主库中；即使后续标记提交失败也保留已成功发布的正确副本。
        pending.committed = true;
    }
    // 所有来源均已确定且复制完成才标记。以后主动删除图片，不再从旧副本复活。
    db::finish_relocation(dir, root_name)
}

fn available_file(path: &Path) -> Result<Option<u64>> {
    crate::image_collection::ensure_local_file_or_missing(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.file_attributes() & UNAVAILABLE == 0 => {
            Ok(Some(metadata.len()))
        }
        Ok(_) => Err("贴图资源不是本地普通文件，补齐已停止".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn directory_guard(path: &Path) -> Result<File> {
    let directory = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & UNAVAILABLE != 0 {
        return Err("贴图资源目录不可安全读取，补齐已停止".into());
    }
    Ok(directory)
}

struct Source {
    file: File,
    bytes: u64,
    _directory: File,
}
impl Source {
    fn open(path: &Path) -> Result<Option<Self>> {
        let Some(length) = available_file(path)? else {
            return Ok(None);
        };
        if length >= MAX_BYTES {
            return Err("旧贴图资源超过工作区容量限制".into());
        }
        let directory = directory_guard(path.parent().ok_or("旧贴图资源没有父目录")?)?;
        let file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.file_attributes() & UNAVAILABLE != 0
            || metadata.len() != length
        {
            return Err("旧贴图资源在读取前改变，补齐已停止".into());
        }
        Ok(Some(Self {
            file,
            bytes: length,
            _directory: directory,
        }))
    }
}

fn same_content(left: &mut Source, mut right: Source) -> Result<bool> {
    if left.bytes != right.bytes {
        return Ok(false);
    }
    left.file.seek(SeekFrom::Start(0))?;
    let mut remaining = left.bytes;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let count = remaining.min(left_buffer.len() as u64) as usize;
        left.file.read_exact(&mut left_buffer[..count])?;
        right.file.read_exact(&mut right_buffer[..count])?;
        if left_buffer[..count] != right_buffer[..count] {
            return Ok(false);
        }
        remaining -= count as u64;
    }
    Ok(true)
}

#[cfg(test)]
#[path = "pin_workspace_relocation_tests.rs"]
mod tests;
