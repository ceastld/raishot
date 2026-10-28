//! 截图和贴图共用一级绘制工具顺序；隐藏工具仍由各宿主的更多菜单提供。
//! 仅显式修改设置时保存，不随绘制、滚轮或窗口关闭写磁盘。
use crate::{annotation::Tool, config_json::JsonFile, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 8 * 1024;
const SCHEMA_VERSION: u32 = 1;

pub(crate) const DEFAULT_TOOLS: [Tool; 7] = [
    Tool::Rectangle,
    Tool::Arrow,
    Tool::Pen,
    Tool::Text,
    Tool::Ellipse,
    Tool::Mosaic,
    Tool::PixelEraser,
];

/// Crop/Edit 属于固定操控入口；其余工具均可调整一级可见性和顺序。
pub(crate) const AVAILABLE_TOOLS: [Tool; 22] = [
    Tool::Rectangle,
    Tool::Arrow,
    Tool::Pen,
    Tool::Text,
    Tool::Ellipse,
    Tool::Mosaic,
    Tool::PixelEraser,
    Tool::Triangle,
    Tool::Diamond,
    Tool::Star,
    Tool::Highlighter,
    Tool::HighlighterRect,
    Tool::Polyline,
    Tool::Serial,
    Tool::Watermark,
    Tool::Blur,
    Tool::MosaicBrush,
    Tool::BlurBrush,
    Tool::LocalFill,
    Tool::Spotlight,
    Tool::Eraser,
    Tool::Lasso,
];

/// 持久化 ID 固定，不使用枚举序号、Debug 名称或本地化文案。
pub(crate) fn tool_id(tool: Tool) -> Option<&'static str> {
    Some(match tool {
        Tool::Rectangle => "rectangle",
        Tool::Arrow => "arrow",
        Tool::Pen => "pen",
        Tool::Text => "text",
        Tool::Ellipse => "ellipse",
        Tool::Triangle => "triangle",
        Tool::Diamond => "diamond",
        Tool::Star => "star",
        Tool::Mosaic => "mosaic",
        Tool::Highlighter => "highlighter",
        Tool::HighlighterRect => "highlighterRect",
        Tool::Polyline => "polyline",
        Tool::Serial => "serial",
        Tool::Watermark => "watermark",
        Tool::Blur => "blur",
        Tool::MosaicBrush => "mosaicBrush",
        Tool::BlurBrush => "blurBrush",
        Tool::LocalFill => "localFill",
        Tool::Spotlight => "spotlight",
        Tool::PixelEraser => "pixelEraser",
        Tool::Eraser => "eraser",
        Tool::Lasso => "lasso",
        Tool::Crop | Tool::Edit | Tool::SmartMosaic => return None,
    })
}

pub(crate) fn id_tool(id: &str) -> Option<Tool> {
    AVAILABLE_TOOLS
        .into_iter()
        .find(|tool| tool_id(*tool) == Some(id))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolbarConfig {
    pub tools: Vec<Tool>,
    /// 同类工具合并为一个带悬停下拉的按钮；关闭时每个工具单独占一格。
    pub grouped: bool,
}
impl Default for ToolbarConfig {
    fn default() -> Self {
        Self {
            tools: DEFAULT_TOOLS.to_vec(),
            grouped: true,
        }
    }
}
impl ToolbarConfig {
    pub(crate) fn load() -> Self {
        Self::load_at(config_path())
    }

    /// 缺失、损坏或不可读取时回退默认；损坏文件由 config_recovery 备份。
    pub(crate) fn load_at(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        match json_file(path).load() {
            Ok(file) => file.map(ToolbarFile::config).unwrap_or_default(),
            Err(error) => {
                crate::config_recovery::report_read_failure(path, &error);
                Self::default()
            }
        }
    }

    /// 保留首次出现的工具顺序，排除固定入口；空列表有意表示不显示绘制工具。
    pub(crate) fn normalized(&self) -> Self {
        Self {
            tools: normalized_tools(&self.tools),
            grouped: self.grouped,
        }
    }

    /// 主栏实际显示的绘制槽位。
    pub(crate) fn slots(&self) -> Vec<crate::toolbar_groups::Slot> {
        crate::toolbar_groups::slots(&self.tools, self.grouped)
    }

    pub(crate) fn save(&self) -> Result<()> {
        self.save_at(config_path())
    }

    /// 跨进程锁忙时短暂重试后报错；同目录临时文件写完后原子替换，错误不改原配置。
    pub(crate) fn save_at(&self, path: impl AsRef<Path>) -> Result<()> {
        Ok(json_file(path.as_ref()).replace(&ToolbarFile::from(&self.normalized()))?)
    }
}

/// toolbar.json 的磁盘结构；工具按固定 ID 存，未知或固定入口的 ID 读入时跳过。
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct ToolbarFile {
    tools: Vec<String>,
    grouped: bool,
}
impl Default for ToolbarFile {
    fn default() -> Self {
        Self::from(&ToolbarConfig::default())
    }
}
impl From<&ToolbarConfig> for ToolbarFile {
    fn from(config: &ToolbarConfig) -> Self {
        Self {
            tools: config
                .tools
                .iter()
                .filter_map(|tool| tool_id(*tool))
                .map(String::from)
                .collect(),
            grouped: config.grouped,
        }
    }
}
impl ToolbarFile {
    fn config(self) -> ToolbarConfig {
        let tools: Vec<_> = self.tools.iter().filter_map(|id| id_tool(id)).collect();
        ToolbarConfig {
            tools: normalized_tools(&tools),
            grouped: self.grouped,
        }
    }
}

pub(crate) fn normalized_tools(tools: &[Tool]) -> Vec<Tool> {
    let mut normalized = Vec::with_capacity(tools.len().min(AVAILABLE_TOOLS.len()));
    for tool in tools {
        if tool_id(*tool).is_some() && !normalized.contains(tool) {
            normalized.push(*tool);
        }
    }
    normalized
}

fn config_path() -> PathBuf {
    crate::preferences::data_dir().join("toolbar.json")
}

fn json_file(path: &Path) -> JsonFile<'_, ToolbarFile> {
    JsonFile {
        path,
        max_bytes: MAX_FILE_BYTES,
        schema_version: SCHEMA_VERSION,
        legacy: |text| ToolbarFile::from(&parse_legacy_ini(text)),
    }
}

/// 旧 toolbar.ini 解析器，只用于迁移；下个版本删除。
fn parse_legacy_ini(text: &str) -> ToolbarConfig {
    let mut in_toolbar = false;
    let mut tools = None;
    let mut version = None;
    let mut grouped = None;
    let mut root = true;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            in_toolbar = line == "[toolbar]";
            root = false;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        // 重复字段取第一项，后续未知节或重复字段不能注入/覆盖已识别顺序。
        if root && key == "version" && version.is_none() {
            version = Some(value.parse::<u32>().ok());
        } else if in_toolbar && key == "tools" && tools.is_none() {
            tools = Some(normalized_tools(
                &value
                    .split(',')
                    .filter_map(|id| id_tool(id.trim()))
                    .collect::<Vec<_>>(),
            ));
        } else if in_toolbar && key == "grouped" && grouped.is_none() {
            // 只认 0 关闭；缺失或其它值保持默认合并。
            grouped = Some(value != "0");
        }
    }
    if version.is_some_and(|version| version != Some(1)) {
        return ToolbarConfig::default();
    }
    let mut config = ToolbarConfig::default();
    if let Some(tools) = tools {
        config.tools = tools;
    }
    config.grouped = grouped.unwrap_or(true);
    config
}

#[cfg(test)]
fn encode_legacy_ini(config: &ToolbarConfig) -> String {
    let ids = config
        .tools
        .iter()
        .filter_map(|tool| tool_id(*tool))
        .collect::<Vec<_>>()
        .join(",");
    // 默认合并不写字段，旧版本读取新文件时行为不变。
    let grouped = if config.grouped { "" } else { "grouped=0\n" };
    format!("version=1\n[toolbar]\ntools={ids}\n{grouped}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_fill_toolbar_id_is_fixed_and_old_default_order_stays_unchanged() {
        assert_eq!(tool_id(Tool::LocalFill), Some("localFill"));
        assert_eq!(id_tool("localFill"), Some(Tool::LocalFill));
        assert_eq!(id_tool("LocalFill"), None);
        assert_eq!(ToolbarConfig::default().tools, DEFAULT_TOOLS);
        let config =
            parse_legacy_ini("version=1\n[toolbar]\ntools=mosaic,localFill,blur,localFill,pen\n");
        assert_eq!(
            config.tools,
            vec![Tool::Mosaic, Tool::LocalFill, Tool::Blur, Tool::Pen]
        );
        assert_eq!(parse_legacy_ini(&encode_legacy_ini(&config)), config);
    }

    #[test]
    fn toolbar_order_ignores_unknown_duplicate_and_fixed_tools_but_keeps_empty() {
        let config = parse_legacy_ini("\u{feff}version=1\n[future]\ntools=text\n[toolbar]\ntools=pen, future,arrow,pen,crop,edit,ellipse\ntools=text\nunknown=anything\n");
        assert_eq!(config.tools, vec![Tool::Pen, Tool::Arrow, Tool::Ellipse]);
        assert!(parse_legacy_ini("version=1\n[toolbar]\ntools=\n")
            .tools
            .is_empty());
        assert_eq!(
            parse_legacy_ini("version=99\n[toolbar]\ntools=pen\n"),
            ToolbarConfig::default()
        );
        assert_eq!(
            parse_legacy_ini("version=garbage\n[toolbar]\ntools=pen\n"),
            ToolbarConfig::default()
        );
        assert_eq!(
            parse_legacy_ini("[toolbar]\nfuture=1\n"),
            ToolbarConfig::default()
        );
        let all = ToolbarConfig {
            tools: AVAILABLE_TOOLS.to_vec(),
            grouped: true,
        };
        assert_eq!(parse_legacy_ini(&encode_legacy_ini(&all)), all);
        let separate = ToolbarConfig {
            grouped: false,
            ..all.clone()
        };
        assert_eq!(parse_legacy_ini(&encode_legacy_ini(&separate)), separate);
        assert!(encode_legacy_ini(&separate).ends_with("grouped=0\n"));
        assert!(!encode_legacy_ini(&all).contains("grouped"));
        assert!(parse_legacy_ini("version=1\n[toolbar]\ntools=pen\ngrouped=yes\n").grouped);
        assert!(!parse_legacy_ini("version=1\n[toolbar]\ngrouped=0\n").grouped);
        for tool in AVAILABLE_TOOLS {
            assert_eq!(id_tool(tool_id(tool).unwrap()), Some(tool));
        }
    }

    #[test]
    fn toolbar_file_round_trip_is_atomic_bounded_and_reports_busy_lock() {
        let dir = crate::config_json::TestDir::new("toolbar-config");
        let path = dir.join("toolbar.json");
        let original = ToolbarConfig {
            tools: vec![Tool::Lasso, Tool::Text, Tool::Pen],
            grouped: true,
        };
        original.save_at(&path).unwrap();
        assert_eq!(ToolbarConfig::load_at(&path), original);
        let file = std::fs::File::open(path.with_extension("ini.lock")).unwrap();
        file.try_lock().unwrap();
        let empty = ToolbarConfig {
            tools: Vec::new(),
            grouped: true,
        };
        assert!(empty.save_at(&path).is_err());
        assert_eq!(ToolbarConfig::load_at(&path), original);
        drop(file);
        empty.save_at(&path).unwrap();
        assert_eq!(ToolbarConfig::load_at(&path), empty);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"schema_version\": 1,\n  \"tools\": [],\n  \"grouped\": true\n}\n"
        );
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(ToolbarConfig::load_at(&path), ToolbarConfig::default());
        std::fs::write(&path, vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert_eq!(ToolbarConfig::load_at(&path), ToolbarConfig::default());
    }

    #[test]
    fn toolbar_json_skips_unknown_ids_and_invalid_fields() {
        let dir = crate::config_json::TestDir::new("toolbar-config");
        let path = dir.join("toolbar.json");
        std::fs::write(
            &path,
            r#"{"schema_version":2,"tools":["pen","future","crop","arrow","pen"],"grouped":"no"}"#,
        )
        .unwrap();
        assert_eq!(
            ToolbarConfig::load_at(&path),
            ToolbarConfig {
                tools: vec![Tool::Pen, Tool::Arrow],
                grouped: true,
            }
        );
        std::fs::write(&path, r#"{"grouped":false}"#).unwrap();
        let loaded = ToolbarConfig::load_at(&path);
        assert_eq!(loaded.tools, DEFAULT_TOOLS);
        assert!(!loaded.grouped);
    }

    #[test]
    fn toolbar_migrates_legacy_ini_to_json() {
        let dir = crate::config_json::TestDir::new("toolbar-config");
        let path = dir.join("toolbar.json");
        let legacy = ToolbarConfig {
            tools: vec![Tool::Mosaic, Tool::LocalFill, Tool::Text],
            grouped: false,
        };
        std::fs::write(dir.join("toolbar.ini"), encode_legacy_ini(&legacy)).unwrap();
        assert_eq!(ToolbarConfig::load_at(&path), legacy);
        assert!(path.exists() && dir.join("toolbar.ini.bak").exists());
        assert!(!dir.join("toolbar.ini").exists());
        assert_eq!(ToolbarConfig::load_at(&path), legacy);
    }
}
