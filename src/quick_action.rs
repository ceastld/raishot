//! 快捷动作纯模型；持久化和启动通道只使用固定的枚举编码。
use crate::{hotkey::HotkeySpec, i18n::Text};
use std::fmt;

pub const MAX_ACTIONS: usize = 64;
pub const MAX_ACTION_HOTKEYS: usize = 16;
pub const MAX_ID_BYTES: usize = 64;
pub const MAX_NAME_BYTES: usize = 256;
const MAGIC: u32 = 0xa500_0000;
const RESERVED: u32 = 0x00fe_0000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Source {
    #[default]
    Region = 0,
    LastRegion = 1,
    CurrentScreen = 2,
    AllScreens = 3,
    ActiveWindow = 4,
}
impl Source {
    pub const ALL: [Self; 5] = [
        Self::Region,
        Self::LastRegion,
        Self::CurrentScreen,
        Self::AllScreens,
        Self::ActiveWindow,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Region => tr!("框选", "Region"),
            Self::LastRegion => tr!("上次区域", "Last region"),
            Self::CurrentScreen => tr!("当前屏幕", "Screen"),
            Self::AllScreens => tr!("所有屏幕", "All screens"),
            Self::ActiveWindow => tr!("当前窗口", "Window"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Step {
    CopyImage = 1,
    Pin = 2,
    QuickSave = 3,
    OcrCopy = 4,
    TableCopy = 5,
    QrCopy = 6,
    QrOpen = 7,
    TranslatePin = 8,
}
impl Step {
    pub const ALL: [Self; 8] = [
        Self::CopyImage,
        Self::Pin,
        Self::QuickSave,
        Self::OcrCopy,
        Self::TableCopy,
        Self::QrCopy,
        Self::QrOpen,
        Self::TranslatePin,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::CopyImage => tr!("复制图片", "Copy image"),
            Self::Pin => tr!("贴图", "Pin"),
            Self::QuickSave => tr!("快速保存", "Quick save"),
            Self::OcrCopy => tr!("识别并复制文字", "OCR → copy"),
            Self::TableCopy => tr!("识别表格并复制", "Table → copy"),
            Self::QrCopy => tr!("扫码复制", "QR → copy"),
            Self::QrOpen => tr!("扫码打开", "QR → open"),
            Self::TranslatePin => tr!("翻译后贴图", "Translate → pin"),
        }
    }
    pub const fn writes_clipboard(self) -> bool {
        matches!(
            self,
            Self::CopyImage | Self::OcrCopy | Self::TableCopy | Self::QrCopy
        )
    }
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Pin | Self::TranslatePin | Self::QrOpen)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HistoryPolicy {
    #[default]
    Inherit,
    Never,
}
impl HistoryPolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Inherit => tr!("跟随设置", "Follow setting"),
            Self::Never => tr!("不写入", "Never"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuickRecipe {
    pub source: Source,
    pub edit: bool,
    pub steps: [Option<Step>; 3],
    pub history: HistoryPolicy,
}
impl Default for QuickRecipe {
    fn default() -> Self {
        Self {
            source: Source::Region,
            edit: true,
            steps: [Some(Step::CopyImage), None, None],
            history: HistoryPolicy::Inherit,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeError {
    Empty,
    StepGap,
    ClipboardConflict,
    TerminalNotLast,
    InvalidMagic,
    ReservedBits,
    UnknownSource,
    UnknownStep,
    InvalidNumber,
}
impl fmt::Display for RecipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => tr!("至少选择一个步骤", "Choose at least one step"),
            Self::StepGap => tr!("步骤必须连续排列", "Steps must be consecutive"),
            Self::ClipboardConflict => tr!(
                "只能有一个写入剪贴板的步骤",
                "Only one step may write to the clipboard"
            ),
            Self::TerminalNotLast => tr!(
                "贴图、翻译贴图或扫码打开必须是最后一步",
                "Pin, translate and QR open must be the last step"
            ),
            Self::InvalidMagic => tr!("快捷动作编码标识无效", "Invalid quick action signature"),
            Self::ReservedBits => tr!(
                "快捷动作编码包含不支持的字段",
                "Unsupported quick action fields"
            ),
            Self::UnknownSource => tr!("不支持这个采集源", "Unsupported capture source"),
            Self::UnknownStep => tr!("不支持这个动作步骤", "Unsupported action step"),
            Self::InvalidNumber => tr!(
                "快捷动作编码必须是 u32 十进制或十六进制数",
                "Quick action code must be a decimal or hexadecimal u32"
            ),
        })
    }
}
impl std::error::Error for RecipeError {}

impl QuickRecipe {
    pub fn steps_iter(&self) -> impl Iterator<Item = Step> + '_ {
        self.steps.iter().copied().flatten()
    }
    pub fn validate(&self) -> std::result::Result<(), RecipeError> {
        let count = self.steps_iter().count();
        if count == 0 {
            return Err(RecipeError::Empty);
        }
        if self.steps[..count].iter().any(Option::is_none) {
            return Err(RecipeError::StepGap);
        }
        if self
            .steps_iter()
            .filter(|step| step.writes_clipboard())
            .count()
            > 1
        {
            return Err(RecipeError::ClipboardConflict);
        }
        if self.steps[..count - 1]
            .iter()
            .flatten()
            .any(|step| step.is_terminal())
        {
            return Err(RecipeError::TerminalNotLast);
        }
        Ok(())
    }
    pub fn pack(&self) -> std::result::Result<u32, RecipeError> {
        self.validate()?;
        let mut code = MAGIC | self.source as u32 | (u32::from(self.edit) << 3);
        for (slot, step) in self.steps.iter().enumerate() {
            if let Some(step) = step {
                code |= (*step as u32) << (4 + slot * 4);
            }
        }
        if self.history == HistoryPolicy::Never {
            code |= 1 << 16;
        }
        Ok(code)
    }
    /// 0 是旧截图通道；其它编码必须完整通过字段与组合校验。
    pub fn unpack(code: u32) -> std::result::Result<Option<Self>, RecipeError> {
        if code == 0 {
            return Ok(None);
        }
        if code & 0xff00_0000 != MAGIC {
            return Err(RecipeError::InvalidMagic);
        }
        if code & RESERVED != 0 {
            return Err(RecipeError::ReservedBits);
        }
        let source = match code & 7 {
            0 => Source::Region,
            1 => Source::LastRegion,
            2 => Source::CurrentScreen,
            3 => Source::AllScreens,
            4 => Source::ActiveWindow,
            _ => return Err(RecipeError::UnknownSource),
        };
        let mut steps = [None; 3];
        for (slot, step) in steps.iter_mut().enumerate() {
            *step = match (code >> (4 + slot * 4)) & 15 {
                0 => None,
                1 => Some(Step::CopyImage),
                2 => Some(Step::Pin),
                3 => Some(Step::QuickSave),
                4 => Some(Step::OcrCopy),
                5 => Some(Step::TableCopy),
                6 => Some(Step::QrCopy),
                7 => Some(Step::QrOpen),
                8 => Some(Step::TranslatePin),
                _ => return Err(RecipeError::UnknownStep),
            };
        }
        let recipe = Self {
            source,
            edit: code & 8 != 0,
            steps,
            history: if code & (1 << 16) == 0 {
                HistoryPolicy::Inherit
            } else {
                HistoryPolicy::Never
            },
        };
        recipe.validate()?;
        Ok(Some(recipe))
    }
    pub fn parse_code(value: &str) -> std::result::Result<Option<Self>, RecipeError> {
        let value = value.trim();
        let code = if let Some(hex) = value
            .strip_prefix("0x")
            .or_else(|| value.strip_prefix("0X"))
        {
            u32::from_str_radix(hex, 16)
        } else {
            value.parse()
        }
        .map_err(|_| RecipeError::InvalidNumber)?;
        Self::unpack(code)
    }
    pub fn summary(&self) -> String {
        let mut labels = vec![self.source.label()];
        if self.edit {
            labels.push(tr!("编辑", "Edit"));
        }
        labels.extend(self.steps_iter().map(Step::label));
        labels.join(" → ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuickAction {
    pub id: String,
    pub name: String,
    pub recipe: QuickRecipe,
    pub hotkey: Option<HotkeySpec>,
    pub in_tray: bool,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionError {
    InvalidId,
    InvalidName,
    Unknown(String),
    Disabled(String),
    DuplicateId(String),
    TooManyActions,
    TooManyHotkeys,
    InvalidHotkey(crate::hotkey::HotkeyError),
    Recipe(RecipeError),
}
impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId => f.write_str(tr!(
                "动作 ID 只能包含字母、数字、连字符和下划线，最长 64 字节",
                "Action IDs use letters, digits, hyphens and underscores, up to 64 bytes"
            )),
            Self::InvalidName => f.write_str(tr!(
                "动作名称不能为空、过长或包含控制字符",
                "Action name cannot be empty, too long or contain control characters"
            )),
            Self::Unknown(id) => write!(f, "{}: {id}", tr!("未知快捷动作", "Unknown quick action")),
            Self::Disabled(id) => write!(
                f,
                "{}: {id}",
                tr!("快捷动作已禁用", "Quick action is disabled")
            ),
            Self::DuplicateId(id) => {
                write!(f, "{}: {id}", tr!("动作 ID 重复", "Duplicate action ID"))
            }
            Self::TooManyActions => f.write_str(tr!(
                "最多保存 64 个快捷动作",
                "At most 64 quick actions can be saved"
            )),
            Self::TooManyHotkeys => f.write_str(tr!(
                "最多绑定 16 个快捷动作热键",
                "At most 16 quick action hotkeys can be assigned"
            )),
            Self::InvalidHotkey(error) => error.fmt(f),
            Self::Recipe(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for ActionError {}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}
pub fn valid_name(name: &str) -> bool {
    !name.trim().is_empty() && name.len() <= MAX_NAME_BYTES && !name.chars().any(char::is_control)
}
impl QuickAction {
    pub fn validate(&self) -> std::result::Result<(), ActionError> {
        if !valid_id(&self.id) {
            return Err(ActionError::InvalidId);
        }
        if !valid_name(&self.name) {
            return Err(ActionError::InvalidName);
        }
        self.recipe.validate().map_err(ActionError::Recipe)?;
        if let Some(hotkey) = self.hotkey {
            hotkey.validate().map_err(ActionError::InvalidHotkey)?;
        }
        Ok(())
    }
    pub fn label(&self) -> &str {
        if let Some(preset) = BUILTIN_PRESETS.iter().find(|preset| preset.id == self.id) {
            if self.name == preset.name.0 {
                return preset.name.get();
            }
        }
        &self.name
    }
    pub fn is_builtin(&self) -> bool {
        BUILTIN_PRESETS.iter().any(|preset| preset.id == self.id)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct BuiltinPreset {
    pub id: &'static str,
    pub name: Text,
    pub recipe: QuickRecipe,
}
const fn preset(
    id: &'static str,
    zh: &'static str,
    en: &'static str,
    source: Source,
    edit: bool,
    first: Step,
    second: Option<Step>,
    history: HistoryPolicy,
) -> BuiltinPreset {
    BuiltinPreset {
        id,
        name: Text(zh, en),
        recipe: QuickRecipe {
            source,
            edit,
            steps: [Some(first), second, None],
            history,
        },
    }
}
pub const BUILTIN_PRESETS: [BuiltinPreset; 13] = [
    preset(
        "capture",
        "截图",
        "Capture",
        Source::Region,
        true,
        Step::CopyImage,
        None,
        HistoryPolicy::Inherit,
    ),
    preset(
        "copy",
        "框选即复制",
        "Select & copy",
        Source::Region,
        false,
        Step::CopyImage,
        None,
        HistoryPolicy::Inherit,
    ),
    preset(
        "pin",
        "框选即贴图",
        "Select & pin",
        Source::Region,
        false,
        Step::Pin,
        None,
        HistoryPolicy::Inherit,
    ),
    preset(
        "copy-pin",
        "复制并贴图",
        "Copy & pin",
        Source::Region,
        false,
        Step::CopyImage,
        Some(Step::Pin),
        HistoryPolicy::Inherit,
    ),
    preset(
        "quick-save",
        "框选即保存",
        "Select & save",
        Source::Region,
        false,
        Step::QuickSave,
        None,
        HistoryPolicy::Inherit,
    ),
    preset(
        "ocr-copy",
        "识别并复制文字",
        "OCR → copy",
        Source::Region,
        false,
        Step::OcrCopy,
        None,
        HistoryPolicy::Never,
    ),
    preset(
        "table-copy",
        "识别表格并复制",
        "Table → copy",
        Source::Region,
        false,
        Step::TableCopy,
        None,
        HistoryPolicy::Never,
    ),
    preset(
        "translate-pin",
        "图片翻译贴图",
        "Translate → pin",
        Source::Region,
        false,
        Step::TranslatePin,
        None,
        HistoryPolicy::Inherit,
    ),
    preset(
        "qr-copy",
        "扫码复制",
        "QR → copy",
        Source::Region,
        false,
        Step::QrCopy,
        None,
        HistoryPolicy::Never,
    ),
    preset(
        "qr-open",
        "扫码打开",
        "QR → open",
        Source::Region,
        false,
        Step::QrOpen,
        None,
        HistoryPolicy::Never,
    ),
    preset(
        "last-copy",
        "上次区域复制",
        "Repeat last → copy",
        Source::LastRegion,
        false,
        Step::CopyImage,
        None,
        HistoryPolicy::Inherit,
    ),
    preset(
        "screen-save",
        "当前屏保存",
        "Screen → save",
        Source::CurrentScreen,
        false,
        Step::QuickSave,
        None,
        HistoryPolicy::Inherit,
    ),
    preset(
        "window-save",
        "当前窗口保存",
        "Window → save",
        Source::ActiveWindow,
        false,
        Step::QuickSave,
        None,
        HistoryPolicy::Inherit,
    ),
];
pub fn builtin_actions() -> Vec<QuickAction> {
    BUILTIN_PRESETS
        .iter()
        .map(|preset| QuickAction {
            id: preset.id.into(),
            name: preset.name.0.into(),
            recipe: preset.recipe,
            hotkey: None,
            in_tray: true,
            enabled: true,
        })
        .collect()
}
pub fn builtin_action(id: &str) -> Option<QuickAction> {
    builtin_actions().into_iter().find(|action| action.id == id)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyOwner {
    Capture,
    ClipboardPin,
    Action(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotkeyConflict {
    pub first: HotkeyOwner,
    pub second: HotkeyOwner,
    pub hotkey: HotkeySpec,
}

pub fn hotkey_conflicts(
    actions: &[QuickAction],
    capture: Option<HotkeySpec>,
    clipboard_pin: Option<HotkeySpec>,
) -> Vec<HotkeyConflict> {
    let mut assigned = Vec::new();
    if let Some(spec) = capture {
        assigned.push((HotkeyOwner::Capture, spec));
    }
    if let Some(spec) = clipboard_pin {
        assigned.push((HotkeyOwner::ClipboardPin, spec));
    }
    for action in actions.iter().filter(|action| action.enabled) {
        if let Some(spec) = action.hotkey {
            assigned.push((HotkeyOwner::Action(action.id.clone()), spec));
        }
    }
    let mut conflicts = Vec::new();
    for (index, (second, spec)) in assigned.iter().enumerate() {
        for (first, earlier) in &assigned[..index] {
            if spec == earlier {
                conflicts.push(HotkeyConflict {
                    first: first.clone(),
                    second: second.clone(),
                    hotkey: *spec,
                });
            }
        }
    }
    conflicts
}
