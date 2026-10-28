//! 开机启动：当前用户 Run 项登记托盘入口 `<品牌>.exe --tray`，以 Windows“启动应用”的关闭标记为准。
use crate::Result;
use std::path::{Path, PathBuf};
use windows::{
    core::{HSTRING, PCWSTR},
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA},
        System::Registry::*,
    },
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
use crate::brand::{AUTOSTART_VALUE as VALUE_NAME, LEGACY_NAMES, RESIDENT_EXE};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AutostartState {
    Off,
    On,
    /// 已登记但指向另一份程序，携带登记的程序路径。
    OtherLocation(String),
    /// 已登记但在任务管理器或系统设置的“启动应用”中被关闭。
    BlockedBySystem,
}

struct Keys {
    run: HSTRING,
    approved: HSTRING,
    value: HSTRING,
    /// 曾用名下的启动项值名，托盘启动时迁到 `value`。
    legacy: Vec<HSTRING>,
}
impl Keys {
    fn system() -> Self {
        Self::at(RUN_KEY, APPROVED_KEY)
    }
    fn at(run: &str, approved: &str) -> Self {
        Self {
            run: run.into(),
            approved: approved.into(),
            value: VALUE_NAME.into(),
            legacy: LEGACY_NAMES.iter().map(|&name| name.into()).collect(),
        }
    }
}

pub(crate) fn command_line(exe: &Path) -> String {
    format!("\"{}\" --tray", exe.display())
}

fn program_of(command: &str) -> &str {
    let command = command.trim_start();
    if let Some(rest) = command.strip_prefix('"') {
        return rest.split('"').next().unwrap_or(rest);
    }
    // 未加引号的路径可能含空格，按首个 .exe 截断。
    match command.to_ascii_lowercase().find(".exe") {
        Some(end) => &command[..end + 4],
        None => command.split(char::is_whitespace).next().unwrap_or(command),
    }
}

fn same_program(program: &str, exe: &Path) -> bool {
    let normalize = |text: &str| text.trim().replace('/', "\\").to_lowercase();
    normalize(program) == normalize(&exe.to_string_lossy())
}

/// StartupApproved 首字节为奇数表示用户已关闭该启动项。
fn approval_blocked(data: Option<&[u8]>) -> bool {
    data.and_then(|data| data.first())
        .is_some_and(|flag| flag & 1 == 1)
}

pub(crate) fn classify(command: Option<&str>, blocked: bool, exe: &Path) -> AutostartState {
    match command {
        None => AutostartState::Off,
        Some(_) if blocked => AutostartState::BlockedBySystem,
        Some(command) => {
            let program = program_of(command);
            if same_program(program, exe) {
                AutostartState::On
            } else {
                AutostartState::OtherLocation(program.to_owned())
            }
        }
    }
}

fn resident_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.with_file_name(RESIDENT_EXE);
    if !exe.is_file() {
        return Err(trf!("未找到 {}", "Not found: {}", exe.display()).into());
    }
    Ok(exe)
}

pub(crate) fn state() -> Result<AutostartState> {
    state_at(&Keys::system(), &resident_exe()?)
}

/// 开启时写入当前程序位置并清除系统关闭标记；关闭时删除本程序的启动项。
pub(crate) fn set(enabled: bool) -> Result<AutostartState> {
    let keys = Keys::system();
    let exe = resident_exe()?;
    set_at(&keys, &exe, enabled)?;
    state_at(&keys, &exe)
}

/// 托盘启动时把已失效的登记改指当前程序；另一份仍存在的程序及系统关闭标记保持原样。
pub(crate) fn repair_moved() -> Result<bool> {
    let exe = std::env::current_exe()?;
    if !exe
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case(RESIDENT_EXE))
    {
        return Ok(false);
    }
    let keys = Keys::system();
    let migrated = migrate_legacy_at(&keys, &exe)?;
    Ok(repair_at(&keys, &exe)? || migrated)
}

/// 曾用名的启动项：指向曾用名托盘程序时删除；未被系统关闭且当前名还没有登记时，改登记为当前程序。
fn migrate_legacy_at(keys: &Keys, exe: &Path) -> Result<bool> {
    let mut changed = false;
    for legacy in &keys.legacy {
        let Some(command) = read_command(&keys.run, legacy)? else {
            continue;
        };
        let legacy_exe = format!("{}.exe", legacy.to_string_lossy());
        if !Path::new(program_of(&command))
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case(&legacy_exe))
        {
            continue;
        }
        let blocked =
            approval_blocked(read_value(&keys.approved, legacy, RRF_RT_REG_BINARY)?.as_deref());
        if !blocked && read_command(&keys.run, &keys.value)?.is_none() {
            write_command(&keys.run, &keys.value, &command_line(exe))?;
        }
        delete_value(&keys.run, legacy)?;
        delete_value(&keys.approved, legacy)?;
        changed = true;
    }
    Ok(changed)
}

fn state_at(keys: &Keys, exe: &Path) -> Result<AutostartState> {
    let command = read_command(&keys.run, &keys.value)?;
    let approval = read_value(&keys.approved, &keys.value, RRF_RT_REG_BINARY)?;
    Ok(classify(
        command.as_deref(),
        approval_blocked(approval.as_deref()),
        exe,
    ))
}

fn set_at(keys: &Keys, exe: &Path, enabled: bool) -> Result<()> {
    if enabled {
        write_command(&keys.run, &keys.value, &command_line(exe))?;
    } else {
        delete_value(&keys.run, &keys.value)?;
    }
    delete_value(&keys.approved, &keys.value)
}

fn repair_at(keys: &Keys, exe: &Path) -> Result<bool> {
    let Some(command) = read_command(&keys.run, &keys.value)? else {
        return Ok(false);
    };
    let program = program_of(&command);
    if program.contains('%') || same_program(program, exe) || Path::new(program).exists() {
        return Ok(false);
    }
    write_command(&keys.run, &keys.value, &command_line(exe))?;
    Ok(true)
}

fn read_command(subkey: &HSTRING, name: &HSTRING) -> Result<Option<String>> {
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    Ok(read_value(subkey, name, flags)?.map(|bytes| {
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .take_while(|&unit| unit != 0)
            .collect();
        String::from_utf16_lossy(&wide)
    }))
}

fn read_value(
    subkey: &HSTRING,
    name: &HSTRING,
    flags: REG_ROUTINE_FLAGS,
) -> Result<Option<Vec<u8>>> {
    let mut data = vec![0_u8; 520];
    for _ in 0..4 {
        let mut size = data.len() as u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey.as_ptr()),
                PCWSTR(name.as_ptr()),
                flags,
                None,
                Some(data.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status == ERROR_MORE_DATA {
            data.resize(size as usize + 2, 0);
            continue;
        }
        status.ok()?;
        data.truncate(size as usize);
        return Ok(Some(data));
    }
    Err("启动项在读取期间持续变化".into())
}

fn write_command(subkey: &HSTRING, name: &HSTRING, command: &str) -> Result<()> {
    let data: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            (data.len() * 2) as u32,
        )
    }
    .ok()?;
    Ok(())
}

fn delete_value(subkey: &HSTRING, name: &HSTRING) -> Result<()> {
    let status = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
        )
    };
    if status != ERROR_FILE_NOT_FOUND {
        status.ok()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_command_line_quotes_program_and_starts_tray_only() {
        let exe = Path::new(r"D:\Program Files\RustShot\rustshot.exe");
        let command = command_line(exe);
        assert_eq!(
            command,
            r#""D:\Program Files\RustShot\rustshot.exe" --tray"#
        );
        assert_eq!(
            program_of(&command),
            r"D:\Program Files\RustShot\rustshot.exe"
        );
        assert_eq!(classify(Some(&command), false, exe), AutostartState::On);
    }

    #[test]
    fn autostart_program_parsing_accepts_quoted_unquoted_and_bare_commands() {
        assert_eq!(program_of(r#"  "C:\a b\x.exe" -autorun"#), r"C:\a b\x.exe");
        assert_eq!(
            program_of(r"C:\a b\RustShot.EXE --tray"),
            r"C:\a b\RustShot.EXE"
        );
        assert_eq!(program_of(r"C:\tools\run --tray"), r"C:\tools\run");
        assert_eq!(
            program_of(r#""C:\截图\rustshot.exe""#),
            r"C:\截图\rustshot.exe"
        );
        assert!(same_program(
            r"c:/截图/RUSTSHOT.exe",
            Path::new(r"C:\截图\rustshot.exe")
        ));
    }

    #[test]
    fn autostart_state_distinguishes_off_other_location_and_system_block() {
        let exe = Path::new(r"C:\RustShot\rustshot.exe");
        assert_eq!(classify(None, true, exe), AutostartState::Off);
        assert_eq!(
            classify(Some(r#""D:\old\rustshot.exe" --tray"#), false, exe),
            AutostartState::OtherLocation(r"D:\old\rustshot.exe".into())
        );
        assert_eq!(
            classify(Some(r#""D:\old\rustshot.exe" --tray"#), true, exe),
            AutostartState::BlockedBySystem
        );
        for (data, blocked) in [
            (None, false),
            (Some(&[][..]), false),
            (Some(&[2, 0, 0, 0][..]), false),
            (Some(&[6, 0, 0, 0][..]), false),
            (Some(&[3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8][..]), true),
            (Some(&[7, 0, 0, 0][..]), true),
        ] {
            assert_eq!(approval_blocked(data), blocked, "{data:?}");
        }
    }

    struct ScratchKey(HSTRING);
    impl Drop for ScratchKey {
        fn drop(&mut self) {
            unsafe {
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(self.0.as_ptr()));
            }
        }
    }

    #[test]
    fn autostart_registry_round_trip_in_scratch_key() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = format!(
            r"Software\RustShot-QA-autostart-{}-{nanos}",
            std::process::id()
        );
        let _scratch = ScratchKey(root.as_str().into());
        let keys = Keys::at(
            &format!(r"{root}\Run"),
            &format!(r"{root}\StartupApproved\Run"),
        );
        let exe = std::env::current_exe().unwrap();
        assert_eq!(state_at(&keys, &exe).unwrap(), AutostartState::Off);

        set_at(&keys, &exe, true).unwrap();
        assert_eq!(state_at(&keys, &exe).unwrap(), AutostartState::On);
        assert_eq!(
            read_command(&keys.run, &keys.value).unwrap().as_deref(),
            Some(command_line(&exe).as_str())
        );

        let disabled: [u8; 12] = [3, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(keys.approved.as_ptr()),
                PCWSTR(keys.value.as_ptr()),
                REG_BINARY.0,
                Some(disabled.as_ptr().cast()),
                disabled.len() as u32,
            )
        }
        .ok()
        .unwrap();
        assert_eq!(
            state_at(&keys, &exe).unwrap(),
            AutostartState::BlockedBySystem
        );
        assert!(!repair_at(&keys, &exe).unwrap());
        set_at(&keys, &exe, true).unwrap();
        assert_eq!(state_at(&keys, &exe).unwrap(), AutostartState::On);
        assert!(read_value(&keys.approved, &keys.value, RRF_RT_REG_BINARY)
            .unwrap()
            .is_none());

        let existing = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let existing_text = existing.to_string_lossy().into_owned();
        write_command(&keys.run, &keys.value, &command_line(&existing)).unwrap();
        assert!(!repair_at(&keys, &exe).unwrap());
        assert_eq!(
            state_at(&keys, &exe).unwrap(),
            AutostartState::OtherLocation(existing_text)
        );

        let moved = r#""Z:\RustShot-QA-missing\rustshot.exe" --tray"#;
        write_command(&keys.run, &keys.value, moved).unwrap();
        assert!(repair_at(&keys, &exe).unwrap());
        assert_eq!(state_at(&keys, &exe).unwrap(), AutostartState::On);

        write_command(
            &keys.run,
            &keys.value,
            r"%RUSTSHOT_QA_MISSING%\rustshot.exe --tray",
        )
        .unwrap();
        assert!(!repair_at(&keys, &exe).unwrap());

        set_at(&keys, &exe, false).unwrap();
        assert_eq!(state_at(&keys, &exe).unwrap(), AutostartState::Off);
        set_at(&keys, &exe, false).unwrap();

        // 曾用名启动项：未关闭的迁为当前名并删除旧值；被系统关闭的只删除；指向别的程序的不动。
        let legacy = keys.legacy.last().unwrap();
        let legacy_command = format!(r#""Z:\old\{}.exe" --tray"#, legacy.to_string_lossy());
        write_command(&keys.run, legacy, &legacy_command).unwrap();
        assert!(migrate_legacy_at(&keys, &exe).unwrap());
        assert_eq!(state_at(&keys, &exe).unwrap(), AutostartState::On);
        assert!(read_command(&keys.run, legacy).unwrap().is_none());

        set_at(&keys, &exe, false).unwrap();
        write_command(&keys.run, legacy, &legacy_command).unwrap();
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(keys.approved.as_ptr()),
                PCWSTR(legacy.as_ptr()),
                REG_BINARY.0,
                Some(disabled.as_ptr().cast()),
                disabled.len() as u32,
            )
        }
        .ok()
        .unwrap();
        assert!(migrate_legacy_at(&keys, &exe).unwrap());
        assert_eq!(state_at(&keys, &exe).unwrap(), AutostartState::Off);
        assert!(read_value(&keys.approved, legacy, RRF_RT_REG_BINARY)
            .unwrap()
            .is_none());

        write_command(&keys.run, legacy, r#""C:\other\tool.exe""#).unwrap();
        assert!(!migrate_legacy_at(&keys, &exe).unwrap());
        assert!(read_command(&keys.run, legacy).unwrap().is_some());
    }
}
