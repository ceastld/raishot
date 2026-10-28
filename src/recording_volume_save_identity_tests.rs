//! 普通 Windows 文件回归：已完成暂存被换成另一文件后，不删除或发布替换物。
//! 只验证生产文件事务的身份门，不初始化 MF，也不表示这些字节是可播放媒体。
#![cfg(windows)]

use super::*;
use crate::file_snapshot;
use std::{
    fs,
    io::{Read, Write},
    os::windows::fs::MetadataExt,
};

const ORIGINAL_BYTES: &[u8] = b"owned completed staging fixture bytes";
const REPLACEMENT_BYTES: &[u8] = b"different file identity; this replacement must survive";
const FILE_LIMIT: u64 = 1024;
const UNSAFE_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;

fn ensure(condition: bool, message: &str, root: &Path) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(format!("{message}；自有产物保留：{}", root.display()).into())
    }
}

fn safe_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure(
        metadata.is_dir() && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0,
        "身份回归目录不是普通本地目录",
        path,
    )
}

fn unique_directory(case: &str) -> Result<PathBuf> {
    let target = std::path::absolute("target")?;
    safe_directory(&target)?;
    let parent = target.join("recording-volume-save-identity");
    match fs::create_dir(&parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    safe_directory(&parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for index in 0..16 {
        let root = parent.join(format!("{case}-{}-{stamp}-{index}", std::process::id()));
        match fs::create_dir(&root) {
            Ok(()) => {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "暂存身份回归自有目录：{}",
                    root.display()
                );
                return Ok(root);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err("暂存身份回归独占目录冲突过多".into())
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    ensure(
        metadata.is_file()
            && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0
            && metadata.len() <= FILE_LIMIT,
        "身份回归文件不是有界普通本地文件",
        path,
    )?;
    let mut bytes = Vec::new();
    File::open(path)?
        .take(FILE_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure(
        bytes.len() as u64 == metadata.len(),
        "回归文件读取期间长度变化",
        path,
    )?;
    Ok(bytes)
}

fn inspect(path: &Path) -> Result<FileSnapshot> {
    file_snapshot::inspect(path)?
        .ok_or_else(|| format!("身份回归文件意外缺失：{}", path.display()).into())
}

struct Evidence {
    root: PathBuf,
    target: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
    original: FileSnapshot,
    replacement: FileSnapshot,
}

impl Evidence {
    fn check_preserved(&self) -> Result<()> {
        ensure(
            inspect(&self.staged)? == self.replacement
                && read_bounded(&self.staged)? == REPLACEMENT_BYTES,
            "事务删除或改变了同路径的新身份替换物",
            &self.root,
        )?;
        ensure(
            inspect(&self.backup)? == self.original
                && read_bounded(&self.backup)? == ORIGINAL_BYTES,
            "事务删除或改变了原暂存备份",
            &self.root,
        )?;
        ensure(
            file_snapshot::inspect(&self.target)?.is_none(),
            "替换的暂存文件被错误发布成最终目标",
            &self.root,
        )
    }
}

fn replaced_staging(case: &str) -> Result<(StagedSave, Evidence)> {
    let root = unique_directory(case)?;
    let target = root.join("final.mp4");
    let request = SaveRequest::for_new_output(&target)?;
    let staged = prepare_with(&request, |path| write_new(path, ORIGINAL_BYTES), || true)?;
    let path = staged.temporary.staged.clone();
    let directory = path.parent().ok_or("实际暂存文件没有父目录")?;
    safe_directory(directory)?;
    ensure(
        directory.parent() == Some(root.as_path()),
        "生产暂存不在本测试的独占根内",
        &root,
    )?;
    let original = inspect(&path)?;
    ensure(
        read_bounded(&path)? == ORIGINAL_BYTES,
        "生产 prepare 没有保留编码闭包的完整暂存字节",
        &root,
    )?;
    let backup = directory.join("owned-staging-backup.mp4");
    // 只换本测试刚创建、已登记完整快照的文件；不查找或移动其它目录内容。
    fs::rename(&path, &backup)?;
    write_new(&path, REPLACEMENT_BYTES)?;
    let replacement = inspect(&path)?;
    ensure(
        replacement.volume != original.volume
            || replacement.id != original.id
            || replacement.created != original.created,
        "替换没有取得不同的文件身份",
        &root,
    )?;
    let evidence = Evidence {
        root,
        target,
        staged: path,
        backup,
        original,
        replacement,
    };
    evidence.check_preserved()?;
    // 无测试 Drop 清理：成功和失败的替换物、备份及独占目录均留给检查。
    Ok((staged, evidence))
}

#[test]
fn recording_volume_save_identity_drop_preserves_replaced_staged_file() -> Result<()> {
    let (staged, evidence) = replaced_staging("drop")?;
    drop(staged);
    evidence.check_preserved()
}

#[test]
fn recording_volume_save_identity_commit_rejects_replaced_staged_file() -> Result<()> {
    let (staged, evidence) = replaced_staging("commit")?;
    // 仍属当前用户意图、最终目标也未被占用；必须因暂存身份变化拒绝发布。
    let outcome = staged.commit(|| true);
    evidence.check_preserved()?;
    ensure(
        outcome.is_err(),
        "当前意图提交没有拒绝被替换的暂存文件",
        &evidence.root,
    )
}

#[test]
fn recording_volume_save_identity_encode_error_keeps_unconfirmed_staged_file_and_path() -> Result<()>
{
    const BYTES: &[u8] = b"unconfirmed encode-error fixture bytes";
    const PRIMARY: &str = "identity-fixture-encode-interrupted";
    let root = unique_directory("encode-error")?;
    let target = root.join("final.mp4");
    let request = SaveRequest::for_new_output(&target)?;
    let mut observed = None;
    let outcome = prepare_with(
        &request,
        |path| {
            write_new(path, BYTES)?;
            // 仅在测试侧记录证据；不写入生产 TemporarySave 的 completed 证明。
            observed = Some((path.to_path_buf(), inspect(path)?));
            Err(io::Error::new(io::ErrorKind::Interrupted, PRIMARY).into())
        },
        || true,
    );
    let error = outcome.err().ok_or("编码闭包返回 Err 后仍取得暂存回执")?;
    let (path, before) = observed.ok_or("编码错误前没有实际写入并观察暂存文件")?;
    ensure(
        path.is_absolute() && path.parent().and_then(Path::parent) == Some(root.as_path()),
        "实际暂存绝对路径不在本测试的独占根内",
        &root,
    )?;
    let message = error.to_string();
    ensure(
        message.contains(PRIMARY) && message.contains(&path.display().to_string()),
        "准备失败诊断丢失了原始错误标识或实际暂存绝对路径",
        &root,
    )?;
    ensure(
        inspect(&path)? == before && read_bounded(&path)? == BYTES,
        "未知完成身份的编码错误产物未按完整快照和字节保留",
        &root,
    )?;
    ensure(
        file_snapshot::inspect(&target)?.is_none(),
        "编码错误产物被发布成最终目标",
        &root,
    )
}

#[test]
fn recording_volume_save_identity_cancel_after_encode_keeps_unconfirmed_replacement_and_backup(
) -> Result<()> {
    let root = unique_directory("cancel-after-encode")?;
    let target = root.join("final.mp4");
    let request = SaveRequest::for_new_output(&target)?;
    let cancelled = std::cell::Cell::new(false);
    let mut observed = None;
    let outcome = prepare_with(
        &request,
        |path| {
            let directory = path.parent().ok_or("实际暂存文件没有父目录")?;
            safe_directory(directory)?;
            ensure(
                path.is_absolute() && directory.parent() == Some(root.as_path()),
                "实际暂存绝对路径不在本测试的独占根内",
                &root,
            )?;
            write_new(path, ORIGINAL_BYTES)?;
            let original = inspect(path)?;
            ensure(
                read_bounded(path)? == ORIGINAL_BYTES,
                "取消前没有写入完整原暂存字节",
                &root,
            )?;
            let backup = directory.join("owned-staging-backup.mp4");
            fs::rename(path, &backup)?;
            write_new(path, REPLACEMENT_BYTES)?;
            let replacement = inspect(path)?;
            ensure(
                replacement.volume != original.volume
                    || replacement.id != original.id
                    || replacement.created != original.created,
                "取消前没有创建异身份替换物",
                &root,
            )?;
            observed = Some(Evidence {
                root: root.clone(),
                target: target.clone(),
                staged: path.to_path_buf(),
                backup,
                original,
                replacement,
            });
            // 不赋值生产 completed：返回 Ok 后的取消门必须先于 open_info。
            cancelled.set(true);
            Ok(())
        },
        || !cancelled.get(),
    );
    let error = outcome.err().ok_or("编码后取消仍取得暂存回执")?;
    let evidence = observed.ok_or("取消前没有实际写入并替换暂存文件")?;
    ensure(cancelled.get(), "编码闭包未实际设置取消状态", &root)?;
    ensure(
        error
            .to_string()
            .contains(&evidence.staged.display().to_string()),
        "编码后取消诊断缺少实际暂存绝对路径",
        &root,
    )?;
    evidence.check_preserved()
}
