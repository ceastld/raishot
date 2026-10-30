//! 本机管理存储的惰性连接；模块加载和路径查询均不创建目录或初始化SQLite。
use crate::Result;
use rusqlite::{Connection, OpenFlags};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

const DATABASE_NAME: &str = "library.sqlite3";
const APPLICATION_ID: i64 = 0x52534854;
const STORAGE_VERSION: i64 = 1;
/// 普通读写遇锁最多等待的时长，界面路径不因数据库长时间卡住。
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_millis(300);

pub(crate) fn path_in(workspace: &Path) -> PathBuf {
    workspace.join(DATABASE_NAME)
}

fn check_path(path: &Path) -> Result<()> {
    crate::image_collection::ensure_local_file_or_missing(path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(suffix);
        crate::image_collection::ensure_local_file_or_missing(Path::new(&sidecar))?;
    }
    Ok(())
}

fn check_identity(connection: &Connection) -> Result<(i64, i64)> {
    let application: i64 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application != 0 && application != APPLICATION_ID {
        return Err("管理数据库属于其它应用，已保留原文件".into());
    }
    if version > STORAGE_VERSION {
        return Err("管理数据库由较新版本创建，请使用新版程序".into());
    }
    Ok((application, version))
}

fn open_connection(path: &Path, flags: OpenFlags) -> Result<Connection> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use std::path::{Component, Prefix};

        // 安全检查先由原路径完成，再规范化已存在的父目录。
        let parent = path.parent().ok_or("管理数据库没有父目录")?;
        let name = path.file_name().ok_or("管理数据库没有文件名")?;
        let mut absolute = std::fs::canonicalize(parent)?.join(name);
        let ordinary = path.components().all(|component| match component {
            Component::Normal(name) => name.to_str().is_some_and(|name| {
                if name.ends_with(['.', ' '])
                    || name
                        .chars()
                        .any(|ch| ch <= '\u{1f}' || "<>:\"/\\|?*".contains(ch))
                {
                    return false;
                }
                let stem = name
                    .split('.')
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches(' ')
                    .to_ascii_uppercase();
                !matches!(
                    stem.as_str(),
                    "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
                ) && !["COM", "LPT"].iter().any(|prefix| {
                    stem.strip_prefix(prefix).is_some_and(|suffix| {
                        matches!(
                            suffix,
                            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                        )
                    })
                })
            }),
            _ => true,
        });
        // 引擎仍会按Win32规则归一化特殊名称，不能让它打开另一条未经检查的路径。
        if !ordinary {
            return Err(tr!(
                "管理数据库目录包含不支持的 Windows 特殊名称",
                "The database folder contains an unsupported Windows name"
            )
            .into());
        }
        if matches!(absolute.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::VerbatimDisk(_)))
        {
            let wide: Vec<u16> = absolute.as_os_str().encode_wide().collect();
            // 短本地路径无需扩展前缀。SQLite会把该前缀误归入UNC共享锁分支，
            // 并发读事务可能互相干扰；预留最长旁文件名和结尾NUL的长度。
            if wide.len() - 4 + "-journal".len() + 1 < 260 {
                absolute = PathBuf::from(std::ffi::OsString::from_wide(&wide[4..]));
            }
        }
        // 必须使用扩展前缀的深目录仍需长路径VFS容纳数据库和旁文件名。
        Ok(Connection::open_with_flags_and_vfs(
            absolute,
            flags,
            "win32-longpath",
        )?)
    }
    #[cfg(not(windows))]
    {
        Ok(Connection::open_with_flags(path, flags)?)
    }
}

/// 实际访问或后台准备已有工作区时调用；首屏和截图待命不打开新库。
pub(crate) fn open_workspace(workspace: &Path) -> Result<Connection> {
    let path = path_in(workspace);
    check_path(&path)?;
    std::fs::create_dir_all(workspace)?;
    let connection = open_connection(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    let (application, version) = check_identity(&connection)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    let journal: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    if !journal.eq_ignore_ascii_case("wal") {
        connection.pragma_update(None, "journal_mode", "WAL")?;
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    if application == 0 {
        connection.pragma_update(None, "application_id", APPLICATION_ID)?;
    }
    if version == 0 {
        connection.pragma_update(None, "user_version", STORAGE_VERSION)?;
    }
    Ok(connection)
}

/// 启动恢复预检只观察已有库，不建目录、建表或执行导入。
pub(crate) fn open_workspace_readonly(workspace: &Path) -> Result<Option<Connection>> {
    let path = path_in(workspace);
    check_path(&path)?;
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let connection = open_connection(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    check_identity(&connection)?;
    Ok(Some(connection))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "rustshot-storage-db-{}-{}", std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
            )))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            assert_eq!(self.0.parent(), Some(std::env::temp_dir().as_path()));
            assert!(self
                .0
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("rustshot-storage-db-"));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn storage_db_startup_readonly_and_path_do_not_create_storage() {
        let fixture = Fixture::new();
        assert_eq!(path_in(&fixture.0), fixture.0.join(DATABASE_NAME));
        assert!(open_workspace_readonly(&fixture.0).unwrap().is_none());
        assert!(!fixture.0.exists());
        std::fs::create_dir(&fixture.0).unwrap();
        std::fs::write(fixture.0.join("workspace.json"), b"legacy source").unwrap();
        assert!(open_workspace_readonly(&fixture.0).unwrap().is_none());
        assert_eq!(std::fs::read_dir(&fixture.0).unwrap().count(), 1);
    }

    #[test]
    fn storage_db_persists_transactions_and_uses_fixed_bundled_engine() {
        let fixture = Fixture::new();
        let connection = open_workspace(&fixture.0).unwrap();
        // 固定的静态SQLite必须包含多进程WAL重置竞态的修复。
        assert!(rusqlite::version_number() >= 3_051_003);
        connection.execute_batch("CREATE TABLE test_rows(value TEXT NOT NULL); BEGIN; INSERT INTO test_rows VALUES('贴图'); COMMIT;").unwrap();
        assert_eq!(
            connection
                .pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(connection);
        let read = open_workspace_readonly(&fixture.0).unwrap().unwrap();
        assert_eq!(
            read.query_row("SELECT value FROM test_rows", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "贴图"
        );
        assert!(read.execute("DELETE FROM test_rows", []).is_err());
    }

    #[test]
    fn storage_db_write_contention_has_bounded_wait_and_preserves_rows() {
        let fixture = Fixture::new();
        let first = open_workspace(&fixture.0).unwrap();
        first.execute_batch("CREATE TABLE test_rows(value INTEGER); INSERT INTO test_rows VALUES(1); BEGIN IMMEDIATE;").unwrap();
        let second = open_workspace(&fixture.0).unwrap();
        let start = Instant::now();
        assert!(second.execute("UPDATE test_rows SET value=2", []).is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
        first.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            second
                .query_row("SELECT value FROM test_rows", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn storage_db_win32_longpath_wal_readwrite_and_readonly() {
        use std::os::windows::ffi::OsStrExt;
        let fixture = Fixture::new();
        let mut workspace = fixture.0.clone();
        while path_in(&workspace).as_os_str().encode_wide().count() < 1200 {
            workspace = workspace.join("storage-deep-目录-".repeat(3));
        }
        let connection = open_workspace(&workspace).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE deep_rows(value TEXT); INSERT INTO deep_rows VALUES('深目录事务');",
            )
            .unwrap();
        let journal: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");
        let read = open_workspace_readonly(&workspace).unwrap().unwrap();
        assert_eq!(
            read.query_row("SELECT value FROM deep_rows", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "深目录事务"
        );
        assert!(read.execute("DELETE FROM deep_rows", []).is_err());
        drop(read);
        drop(connection);
        assert!(path_in(&workspace).is_file());
    }

    #[cfg(windows)]
    #[test]
    fn storage_db_rejects_verbatim_special_directory_without_aliasing_normal_directory() {
        let fixture = Fixture::new();
        std::fs::create_dir_all(&fixture.0).unwrap();
        let parent = std::fs::canonicalize(&fixture.0).unwrap();
        let normal = parent.join("literal");
        let verbatim = parent.join("literal.");
        std::fs::create_dir(&normal).unwrap();
        std::fs::create_dir(&verbatim).unwrap();
        assert!(open_workspace(&verbatim).is_err());
        assert!(open_workspace_readonly(&verbatim).unwrap().is_none());
        assert!(!path_in(&verbatim).exists());
        assert!(!path_in(&normal).exists());
    }

    #[test]
    fn storage_db_independent_readers_keep_snapshots_across_writes_and_staggered_closes() {
        use std::sync::{Arc, Barrier};

        const READERS: usize = 6;
        const ROUNDS: usize = 6;
        let fixture = Fixture::new();
        let connection = open_workspace(&fixture.0).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE concurrent_rows(id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
                 INSERT INTO concurrent_rows VALUES(1,1),(2,2),(3,3);",
            )
            .unwrap();
        drop(connection);

        let barrier = Arc::new(Barrier::new(READERS));
        let failures = std::thread::scope(|scope| {
            let tasks = (0..READERS)
                .map(|reader| {
                    let barrier = barrier.clone();
                    let workspace = fixture.0.clone();
                    scope.spawn(move || {
                        let mut failure = None;
                        for round in 0..ROUNDS {
                            barrier.wait();
                            let mut connection = if failure.is_none() {
                                match open_workspace(&workspace) {
                                    Ok(connection) => Some(connection),
                                    Err(error) => {
                                        failure = Some(format!("open round {round}: {error}"));
                                        None
                                    }
                                }
                            } else {
                                None
                            };
                            let transaction = match connection.as_mut() {
                                Some(connection) => match connection.transaction() {
                                    Ok(transaction) => Some(transaction),
                                    Err(error) => {
                                        failure = Some(format!("begin round {round}: {error}"));
                                        None
                                    }
                                },
                                None => None,
                            };
                            let check = |transaction: &rusqlite::Transaction<'_>| {
                                let values: (i64, i64) = transaction
                                    .query_row(
                                        "SELECT count(*), sum(value) FROM concurrent_rows",
                                        [],
                                        |row| Ok((row.get(0)?, row.get(1)?)),
                                    )
                                    .map_err(|error| error.to_string())?;
                                if values != (3, 6 + round as i64) {
                                    return Err(format!(
                                        "round {round}: unexpected snapshot {values:?}"
                                    ));
                                }
                                Ok(())
                            };
                            if let Some(transaction) = &transaction {
                                if let Err(error) = check(transaction) {
                                    failure = Some(error);
                                }
                            }
                            // 所有读取事务均已取得同一代快照；一半连接先关闭。
                            // 出错也必须参加全部屏障，避免失败时让其它测试线程挂起。
                            barrier.wait();
                            if reader < READERS / 2 {
                                drop(transaction);
                                if reader == 0 && failure.is_none() {
                                    if let Some(connection) = &connection {
                                        if let Err(error) = connection.execute(
                                            "UPDATE concurrent_rows SET value=value+1 WHERE id=1",
                                            [],
                                        ) {
                                            failure = Some(format!("write round {round}: {error}"));
                                        }
                                    }
                                }
                                drop(connection);
                                barrier.wait();
                            } else {
                                barrier.wait();
                                // 新提交不能改变尚未关闭的读取快照。
                                if let Some(transaction) = &transaction {
                                    if let Err(error) = check(transaction) {
                                        failure = Some(error);
                                    }
                                }
                                drop(transaction);
                                drop(connection);
                            }
                            // 最后连接关闭后再同时重开，覆盖SHM生命周期交接。
                            barrier.wait();
                        }
                        failure
                    })
                })
                .collect::<Vec<_>>();
            tasks
                .into_iter()
                .filter_map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(failures.is_empty(), "{failures:?}");
        let connection = open_workspace_readonly(&fixture.0).unwrap().unwrap();
        let sum: i64 = connection
            .query_row("SELECT sum(value) FROM concurrent_rows", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sum, 6 + ROUNDS as i64);
    }
}
