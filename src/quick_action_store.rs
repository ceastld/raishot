//! 快捷动作独立配置；内置项仅保存差异，不修改通用偏好文件。
use crate::{
    config_json::JsonFile,
    hotkey::HotkeySpec,
    quick_action::{
        builtin_action, builtin_actions, valid_id, valid_name, ActionError, QuickAction,
        QuickRecipe, MAX_ACTIONS, MAX_ACTION_HOTKEYS,
    },
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_LINE_BYTES: usize = 1024;
const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickActionStore {
    pub actions: Vec<QuickAction>,
}
impl Default for QuickActionStore {
    fn default() -> Self {
        Self {
            actions: builtin_actions(),
        }
    }
}
impl QuickActionStore {
    pub fn load() -> Self {
        Self::load_at(config_path())
    }
    /// 文件缺失、读取失败或损坏时保留全部内置动作；损坏文件由 config_recovery 备份。
    pub fn load_at(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        match json_file(path).load() {
            Ok(file) => file.map(Self::from_file).unwrap_or_default(),
            Err(error) => {
                crate::config_recovery::report_read_failure(path, &error);
                Self::default()
            }
        }
    }
    pub fn find(&self, id: &str) -> Option<&QuickAction> {
        self.actions.iter().find(|action| action.id == id)
    }
    pub fn find_mut(&mut self, id: &str) -> Option<&mut QuickAction> {
        self.actions.iter_mut().find(|action| action.id == id)
    }
    /// CLI 名称使用完整 ID，未知或禁用项明确报错，避免意外普通截图。
    pub fn resolve(&self, id: &str) -> std::result::Result<QuickRecipe, ActionError> {
        if !valid_id(id) {
            return Err(ActionError::InvalidId);
        }
        let action = self
            .find(id)
            .ok_or_else(|| ActionError::Unknown(id.into()))?;
        if !action.enabled {
            return Err(ActionError::Disabled(id.into()));
        }
        action.validate()?;
        Ok(action.recipe)
    }
    pub fn validate(&self) -> std::result::Result<(), ActionError> {
        if self.actions.len() > MAX_ACTIONS {
            return Err(ActionError::TooManyActions);
        }
        let mut ids = HashSet::new();
        for action in &self.actions {
            action.validate()?;
            if !ids.insert(action.id.as_str()) {
                return Err(ActionError::DuplicateId(action.id.clone()));
            }
        }
        if self
            .actions
            .iter()
            .filter(|action| action.enabled && action.hotkey.is_some())
            .count()
            > MAX_ACTION_HOTKEYS
        {
            return Err(ActionError::TooManyHotkeys);
        }
        Ok(())
    }
    pub fn hotkey_conflicts(
        &self,
        capture: Option<HotkeySpec>,
        clipboard_pin: Option<HotkeySpec>,
    ) -> Vec<crate::quick_action::HotkeyConflict> {
        crate::quick_action::hotkey_conflicts(&self.actions, capture, clipboard_pin)
    }
    /// 逐项套用与旧 INI 相同的校验；无效 ID、名称、步骤或热键只丢弃该项或该字段。
    fn from_file(file: ActionsFile) -> Self {
        let mut store = Self::default();
        for value in file.actions {
            let serde_json::Value::Object(map) = value else {
                continue;
            };
            let entry = crate::config_json::overlay(ActionEntry::default(), map);
            if !valid_id(&entry.id) {
                continue;
            }
            let mut pending = PendingAction::new(&entry.id, &store);
            if let Some(name) = entry.name.filter(|name| valid_name(name)) {
                pending.action.name = name;
            }
            if let Some(Ok(Some(recipe))) = entry.recipe.map(|code| QuickRecipe::parse_code(&code))
            {
                pending.action.recipe = recipe;
                pending.has_recipe = true;
            }
            if let Some(hotkey) = entry.hotkey.and_then(|value| parse_hotkey(&value)) {
                pending.action.hotkey = hotkey;
            }
            if let Some(value) = entry.tray {
                pending.action.in_tray = value;
            }
            if let Some(value) = entry.enabled {
                pending.action.enabled = value;
            }
            finish_section(&mut store, Some(pending));
        }
        limit_hotkeys(&mut store);
        store
    }
    /// 内置项只写与默认不同的字段，未改动的内置项不出现。
    fn file(&self) -> std::result::Result<ActionsFile, ActionError> {
        self.validate()?;
        let mut actions = Vec::new();
        for action in &self.actions {
            let builtin = builtin_action(&action.id);
            macro_rules! differs {
                ($field:ident) => {
                    builtin
                        .as_ref()
                        .is_none_or(|default| action.$field != default.$field)
                };
            }
            let recipe = if differs!(recipe) {
                Some(format!(
                    "0x{:08X}",
                    action.recipe.pack().map_err(ActionError::Recipe)?
                ))
            } else {
                None
            };
            let entry = ActionEntry {
                id: action.id.clone(),
                name: differs!(name).then(|| action.name.trim().to_string()),
                recipe,
                hotkey: differs!(hotkey).then(|| match action.hotkey {
                    Some(spec) => format!("{},{}", spec.modifiers, spec.key),
                    None => "none".into(),
                }),
                tray: differs!(in_tray).then_some(action.in_tray),
                enabled: differs!(enabled).then_some(action.enabled),
            };
            let changed = entry.name.is_some()
                || entry.recipe.is_some()
                || entry.hotkey.is_some()
                || entry.tray.is_some()
                || entry.enabled.is_some();
            if changed {
                if let Ok(value) = serde_json::to_value(&entry) {
                    actions.push(value);
                }
            }
        }
        Ok(ActionsFile { actions })
    }
    /// 旧 quick_actions.ini 解析器，只用于迁移；下个版本删除。
    pub fn parse_legacy_ini(text: &str) -> Self {
        if text.len() as u64 > MAX_FILE_BYTES {
            return Self::default();
        }
        // 没有 version 的首版手工文件也可读；未知显式版本不误当现有布局。
        for line in text
            .lines()
            .take_while(|line| !line.trim().starts_with('['))
        {
            if let Some(("version", value)) = line.trim().split_once('=') {
                if value.trim() != "1" {
                    return Self::default();
                }
            }
        }
        let mut store = Self::default();
        let mut pending: Option<PendingAction> = None;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with([';', '#']) {
                continue;
            }
            if line.starts_with('[') {
                finish_section(&mut store, pending.take());
                pending = line
                    .strip_prefix("[action.")
                    .and_then(|line| line.strip_suffix(']'))
                    .filter(|id| valid_id(id))
                    .map(|id| PendingAction::new(id, &store));
                continue;
            }
            if line.len() > MAX_LINE_BYTES {
                continue;
            }
            let Some(action) = pending.as_mut() else {
                continue;
            };
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "name" if valid_name(value) => action.action.name = value.into(),
                "recipe" => {
                    if let Ok(Some(recipe)) = QuickRecipe::parse_code(value) {
                        action.action.recipe = recipe;
                        action.has_recipe = true;
                    }
                }
                "hotkey" => {
                    if let Some(hotkey) = parse_hotkey(value) {
                        action.action.hotkey = hotkey;
                    }
                }
                "tray" => {
                    if let Some(value) = parse_bool(value) {
                        action.action.in_tray = value;
                    }
                }
                "enabled" => {
                    if let Some(value) = parse_bool(value) {
                        action.action.enabled = value;
                    }
                }
                _ => {}
            }
        }
        finish_section(&mut store, pending);
        limit_hotkeys(&mut store);
        store
    }
    #[cfg(test)]
    pub fn encode_legacy_ini(&self) -> std::result::Result<String, ActionError> {
        self.validate()?;
        let mut text = String::from("version=1\n");
        for action in &self.actions {
            let builtin = builtin_action(&action.id);
            let mut fields = String::new();
            if builtin
                .as_ref()
                .is_none_or(|default| action.name != default.name)
            {
                fields.push_str(&format!("name={}\n", action.name.trim()));
            }
            if builtin
                .as_ref()
                .is_none_or(|default| action.recipe != default.recipe)
            {
                fields.push_str(&format!(
                    "recipe=0x{:08X}\n",
                    action.recipe.pack().map_err(ActionError::Recipe)?
                ));
            }
            if builtin
                .as_ref()
                .is_none_or(|default| action.hotkey != default.hotkey)
            {
                let hotkey = match action.hotkey {
                    Some(spec) => format!("{},{}", spec.modifiers, spec.key),
                    None => "none".into(),
                };
                fields.push_str(&format!("hotkey={hotkey}\n"));
            }
            if builtin
                .as_ref()
                .is_none_or(|default| action.in_tray != default.in_tray)
            {
                fields.push_str(&format!("tray={}\n", action.in_tray));
            }
            if builtin
                .as_ref()
                .is_none_or(|default| action.enabled != default.enabled)
            {
                fields.push_str(&format!("enabled={}\n", action.enabled));
            }
            if !fields.is_empty() {
                text.push_str(&format!("\n[action.{}]\n{fields}", action.id));
            }
        }
        Ok(text)
    }
    pub fn save(&self) -> Result<()> {
        self.save_at(config_path())
    }
    /// 同目录临时文件原子替换；跨进程锁忙时短暂重试后报错。
    pub fn save_at(&self, path: impl AsRef<Path>) -> Result<()> {
        let file = self.file()?;
        Ok(json_file(path.as_ref()).replace(&file)?)
    }
}

/// quick_actions.json 的磁盘结构；每项单独解析，一项损坏不影响其它项。
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct ActionsFile {
    actions: Vec<serde_json::Value>,
}
/// 一项动作；内置项缺省的字段沿用内置值。
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct ActionEntry {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// 与托盘／CLI 相同的打包步骤码，十六进制字符串。
    #[serde(skip_serializing_if = "Option::is_none")]
    recipe: Option<String>,
    /// `修饰键,虚拟键` 或 `none`。
    #[serde(skip_serializing_if = "Option::is_none")]
    hotkey: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tray: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
}

fn config_path() -> PathBuf {
    crate::preferences::data_dir().join("quick_actions.json")
}
fn json_file(path: &Path) -> JsonFile<'_, ActionsFile> {
    JsonFile {
        path,
        max_bytes: MAX_FILE_BYTES,
        schema_version: SCHEMA_VERSION,
        legacy: |text| {
            QuickActionStore::parse_legacy_ini(text)
                .file()
                .unwrap_or_default()
        },
    }
}
/// 文件可手工修改；超出的热键不进入 16 槽注册区，动作本身仍可从托盘运行。
fn limit_hotkeys(store: &mut QuickActionStore) {
    let mut hotkeys = 0;
    for action in &mut store.actions {
        if action.enabled && action.hotkey.is_some() {
            hotkeys += 1;
            if hotkeys > MAX_ACTION_HOTKEYS {
                action.hotkey = None;
            }
        }
    }
}
struct PendingAction {
    action: QuickAction,
    has_recipe: bool,
}
impl PendingAction {
    fn new(id: &str, store: &QuickActionStore) -> Self {
        if let Some(action) = store.find(id) {
            Self {
                action: action.clone(),
                has_recipe: true,
            }
        } else {
            Self {
                action: QuickAction {
                    id: id.into(),
                    name: id.into(),
                    recipe: QuickRecipe::default(),
                    hotkey: None,
                    in_tray: true,
                    enabled: true,
                },
                has_recipe: false,
            }
        }
    }
}
fn finish_section(store: &mut QuickActionStore, pending: Option<PendingAction>) {
    let Some(pending) = pending else {
        return;
    };
    if !pending.has_recipe || pending.action.validate().is_err() {
        return;
    }
    if let Some(action) = store.find_mut(&pending.action.id) {
        *action = pending.action;
    } else if store.actions.len() < MAX_ACTIONS {
        store.actions.push(pending.action);
    }
}
fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}
fn parse_hotkey(value: &str) -> Option<Option<HotkeySpec>> {
    if matches!(value, "none" | "disabled" | "") {
        return Some(None);
    }
    let (modifiers, key) = value.split_once(',')?;
    let spec = HotkeySpec {
        modifiers: modifiers.trim().parse().ok()?,
        key: key.trim().parse().ok()?,
    };
    spec.validate().ok()?;
    Some(Some(spec))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hotkey::CTRL, quick_action::Step};

    fn customized() -> QuickActionStore {
        let mut store = QuickActionStore::default();
        store.find_mut("ocr-copy").unwrap().hotkey = Some(HotkeySpec {
            modifiers: CTRL,
            key: b'3'.into(),
        });
        store.find_mut("copy").unwrap().in_tray = false;
        let mut custom = builtin_action("copy-pin").unwrap();
        custom.id = "save-copy-pin".into();
        custom.name = "保存复制贴图".into();
        custom.recipe.steps = [
            Some(Step::QuickSave),
            Some(Step::CopyImage),
            Some(Step::Pin),
        ];
        store.actions.push(custom);
        store
    }

    #[test]
    fn quick_action_store_json_round_trip_saves_only_differences() {
        let dir = crate::config_json::TestDir::new("quick-actions");
        let path = dir.join("quick_actions.json");
        assert!(QuickActionStore::default()
            .file()
            .unwrap()
            .actions
            .is_empty());
        let store = customized();
        store.save_at(&path).unwrap();
        assert_eq!(QuickActionStore::load_at(&path), store);
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["schema_version"], 1);
        let actions = value["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0], serde_json::json!({"id": "copy", "tray": false}));
        assert!(actions
            .iter()
            .any(|entry| entry == &serde_json::json!({"id": "ocr-copy", "hotkey": "1,51"})));
    }

    #[test]
    fn quick_action_store_json_skips_damaged_entries_and_fields() {
        let dir = crate::config_json::TestDir::new("quick-actions");
        let path = dir.join("quick_actions.json");
        std::fs::write(
            &path,
            r#"{"schema_version":1,"actions":[
                {"id":"copy","name":"自定义复制","tray":"no","hotkey":"17,51","enabled":false},
                {"id":"bad/path","name":"污染名称"},
                {"id":"custom","recipe":"broken"},
                {"id":"custom-ok","recipe":"0xA5000030"},
                42
            ]}"#,
        )
        .unwrap();
        let store = QuickActionStore::load_at(&path);
        let copy = store.find("copy").unwrap();
        assert_eq!(copy.name, "自定义复制");
        assert!(copy.in_tray && !copy.enabled);
        assert_eq!(copy.hotkey, None);
        assert!(store.find("bad/path").is_none() && store.find("custom").is_none());
        assert_eq!(
            store.find("custom-ok").unwrap().recipe.steps[0],
            Some(Step::QuickSave)
        );
    }

    #[test]
    fn quick_action_store_migrates_legacy_ini_to_json() {
        let dir = crate::config_json::TestDir::new("quick-actions");
        let path = dir.join("quick_actions.json");
        let store = customized();
        std::fs::write(
            dir.join("quick_actions.ini"),
            store.encode_legacy_ini().unwrap(),
        )
        .unwrap();
        assert_eq!(QuickActionStore::load_at(&path), store);
        assert!(path.exists() && dir.join("quick_actions.ini.bak").exists());
        assert!(!dir.join("quick_actions.ini").exists());
        assert_eq!(QuickActionStore::load_at(&path), store);
    }
}
