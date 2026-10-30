//! 对外品牌：显示名、EXE 名、数据目录与开机启动项名。
//! 改名只改本文件、`Cargo.toml` 的 `[[bin]]`、`build.rs` 的 GPUI 程序名、`Tools/package.ps1` 和 `installer/raishot.iss`；
//! 内部代号 rustshot（crate、模块、窗口类、互斥量、环境变量）不随品牌改名。
use std::path::PathBuf;
use std::sync::OnceLock;

/// 显示名的字面量，供 `concat!` 拼接窗口标题等静态文案。
#[macro_export]
macro_rules! brand_name {
    () => {
        "RaiShot"
    };
}

/// 带品牌前缀的成对窗口标题：`brand_title!("设置", "Settings")` → “RaiShot · 设置”。
#[macro_export]
macro_rules! brand_title {
    ($zh:literal, $en:literal $(,)?) => {
        $crate::i18n::tr(
            concat!($crate::brand_name!(), " · ", $zh),
            concat!($crate::brand_name!(), " · ", $en),
        )
    };
}

pub const NAME: &str = brand_name!();
pub const RESIDENT_EXE: &str = "raishot.exe";
pub const GPUI_EXE: &str = "raishot-gpui.exe";
pub const CLI_EXE: &str = "raishot-cli.exe";
/// `%LOCALAPPDATA%` 与“图片”下的目录名。
pub const DATA_DIR: &str = brand_name!();
/// 当前用户 Run 项的值名。
pub const AUTOSTART_VALUE: &str = brand_name!();
pub const USER_AGENT: &str = concat!(brand_name!(), "/", env!("CARGO_PKG_VERSION"));
/// 曾用名：只用于把旧数据目录与开机启动项迁到当前品牌。
pub const LEGACY_NAMES: &[&str] = &["RustShot", "Kirie", "RabShot"];

/// `%LOCALAPPDATA%\<品牌>`。新目录还不存在时优先迁移最近曾用名的数据目录；
/// 改名失败（旧版仍在运行、文件被占用）时本次沿用旧目录，下次启动再迁。
pub fn local_data_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        migrate_data_root(&base)
    })
    .clone()
}

fn migrate_data_root(base: &std::path::Path) -> PathBuf {
    let current = base.join(DATA_DIR);
    if current.exists() {
        return current;
    }
    for legacy in LEGACY_NAMES.iter().rev() {
        let old = base.join(legacy);
        if !old.is_dir() {
            continue;
        }
        return match std::fs::rename(&old, &current) {
            Ok(()) => current,
            // 另一进程已抢先迁移。
            Err(_) if current.exists() => current,
            Err(_) => old,
        };
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rustshot-brand-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn brand_data_root_moves_legacy_directory_once_and_keeps_contents() {
        for legacy in LEGACY_NAMES {
            let base = scratch("move");
            std::fs::create_dir_all(base.join(legacy)).unwrap();
            std::fs::write(base.join(legacy).join("settings.ini"), "theme=dark").unwrap();
            let root = migrate_data_root(&base);
            assert_eq!(root, base.join(DATA_DIR));
            assert_eq!(
                std::fs::read_to_string(root.join("settings.ini")).unwrap(),
                "theme=dark"
            );
            assert!(!base.join(legacy).exists());
            // 已迁移后再次调用不再动目录。
            assert_eq!(migrate_data_root(&base), base.join(DATA_DIR));
            std::fs::remove_dir_all(&base).unwrap();
        }
    }

    #[test]
    fn brand_data_root_prefers_existing_current_directory_over_legacy() {
        let base = scratch("both");
        std::fs::create_dir_all(base.join(DATA_DIR)).unwrap();
        for legacy in LEGACY_NAMES {
            std::fs::create_dir_all(base.join(legacy)).unwrap();
        }
        assert_eq!(migrate_data_root(&base), base.join(DATA_DIR));
        for legacy in LEGACY_NAMES {
            assert!(base.join(legacy).exists(), "不合并、不删除曾用名目录");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn brand_data_root_prefers_most_recent_legacy_brand() {
        let base = scratch("latest");
        for legacy in LEGACY_NAMES {
            std::fs::create_dir_all(base.join(legacy)).unwrap();
            std::fs::write(base.join(legacy).join("settings.json"), legacy).unwrap();
        }
        let recent = LEGACY_NAMES.last().unwrap();
        let root = migrate_data_root(&base);
        assert_eq!(root, base.join(DATA_DIR));
        assert_eq!(
            std::fs::read_to_string(root.join("settings.json")).unwrap(),
            *recent
        );
        assert!(!base.join(recent).exists());
        for legacy in &LEGACY_NAMES[..LEGACY_NAMES.len() - 1] {
            assert!(base.join(legacy).exists(), "保留更早品牌的数据，不自动合并");
        }
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn brand_titles_and_user_agent_use_display_name() {
        assert!(USER_AGENT.starts_with(concat!(brand_name!(), "/")));
        assert!(brand_title!("设置", "Settings").starts_with(NAME));
    }
}
