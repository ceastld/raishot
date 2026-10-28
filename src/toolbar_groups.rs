//! 主栏工具分组与数字快捷键：同类工具合并为一个带下拉的按钮，数字按可见槽位分配。
//! 纯模型，不依赖 GPUI；渲染、布局宽度与按键解析读取同一组槽位，避免各自推算。
use crate::{annotation::Tool, i18n::Text};
use std::time::Duration;

/// 普通按钮与分组按钮（主按钮 + 下拉箭头）的逻辑宽度。
pub(crate) const TOOL_WIDTH: f32 = 32.;
pub(crate) const GROUP_WIDTH: f32 = 44.;
/// 主栏拖拽把手宽度；渲染与布局宽度共用，避免把手加宽后末尾按钮折行。
pub(crate) const GRIP_WIDTH: f32 = 14.;
/// 悬停展开与离开收起的延时：快速掠过不弹出，移入下拉不闪断。
pub(crate) const HOVER_OPEN_DELAY: Duration = Duration::from_millis(60);
pub(crate) const HOVER_CLOSE_DELAY: Duration = Duration::from_millis(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ToolGroup {
    Shape,
    Line,
    Mark,
    Text,
    Mosaic,
    Eraser,
}
pub(crate) const GROUPS: [ToolGroup; 6] = [
    ToolGroup::Shape,
    ToolGroup::Line,
    ToolGroup::Mark,
    ToolGroup::Text,
    ToolGroup::Mosaic,
    ToolGroup::Eraser,
];
impl ToolGroup {
    fn index(self) -> usize {
        self as usize
    }
    /// 下拉箭头的 UI 标识；主按钮沿用当前显示工具的标识。
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Shape => "group-shape",
            Self::Line => "group-line",
            Self::Mark => "group-mark",
            Self::Text => "group-text",
            Self::Mosaic => "group-mosaic",
            Self::Eraser => "group-eraser",
        }
    }
    pub(crate) fn label(self) -> Text {
        match self {
            Self::Shape => Text("形状", "Shapes"),
            Self::Line => Text("线条", "Lines"),
            Self::Mark => Text("标记", "Markup"),
            Self::Text => Text("文字", "Text"),
            Self::Mosaic => Text("打码", "Redact"),
            Self::Eraser => Text("橡皮擦", "Erasers"),
        }
    }
    /// 组内顺序即下拉顺序与数字轮换顺序。
    pub(crate) fn members(self) -> &'static [Tool] {
        match self {
            Self::Shape => &[
                Tool::Rectangle,
                Tool::Ellipse,
                Tool::Triangle,
                Tool::Diamond,
                Tool::Star,
            ],
            // 直线、单双箭头等是 Arrow 的预设，折线与曲线箭头同属“线条”。
            Self::Line => &[Tool::Arrow, Tool::Polyline],
            Self::Mark => &[
                Tool::Pen,
                Tool::Highlighter,
                Tool::HighlighterRect,
                Tool::Spotlight,
            ],
            Self::Text => &[Tool::Text, Tool::Serial, Tool::Watermark],
            Self::Mosaic => &[
                Tool::Mosaic,
                Tool::Blur,
                Tool::MosaicBrush,
                Tool::BlurBrush,
                Tool::LocalFill,
                Tool::SmartMosaic,
            ],
            Self::Eraser => &[Tool::PixelEraser, Tool::Eraser],
        }
    }
    /// 智能马赛克只在普通截图可用；图片编辑与贴图不列出。
    pub(crate) fn options(self, smart_mosaic: bool) -> impl Iterator<Item = Tool> {
        self.members()
            .iter()
            .copied()
            .filter(move |tool| smart_mosaic || *tool != Tool::SmartMosaic)
    }
}

pub(crate) fn group_of(tool: Tool) -> Option<ToolGroup> {
    GROUPS
        .into_iter()
        .find(|group| group.members().contains(&tool))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Tool(Tool),
    Group(ToolGroup),
}
impl Slot {
    pub(crate) fn width(self) -> f32 {
        match self {
            Self::Tool(_) => TOOL_WIDTH,
            Self::Group(_) => GROUP_WIDTH,
        }
    }
    pub(crate) fn contains(self, tool: Tool) -> bool {
        match self {
            Self::Tool(own) => own == tool,
            Self::Group(group) => group.members().contains(&tool),
        }
    }
}

/// 配置顺序中首个出现的组员决定分组位置；同组其它已启用工具并入该按钮。
pub(crate) fn slots(tools: &[Tool], grouped: bool) -> Vec<Slot> {
    let mut slots = Vec::new();
    for tool in crate::toolbar_config::normalized_tools(tools) {
        let slot = match group_of(tool).filter(|_| grouped) {
            Some(group) => Slot::Group(group),
            None => Slot::Tool(tool),
        };
        if !slots.contains(&slot) {
            slots.push(slot);
        }
    }
    slots
}

/// 数字 1–9 依次对应主栏绘制槽位；0 固定为鼠标模式，由宿主处理。
pub(crate) fn shortcut_number(index: usize) -> Option<u8> {
    (index < 9).then_some(index as u8 + 1)
}

pub(crate) fn shortcut_digit(key: &str) -> Option<u8> {
    match key.as_bytes() {
        [digit @ b'0'..=b'9'] => Some(digit - b'0'),
        _ => None,
    }
}

/// 每组最近一次使用的工具，作为分组主按钮的默认项；只在本次会话内保留，不写磁盘。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GroupMemory([Option<Tool>; GROUPS.len()]);
impl GroupMemory {
    pub(crate) fn remember(&mut self, tool: Tool) {
        if let Some(group) = group_of(tool) {
            self.0[group.index()] = Some(tool);
        }
    }
    pub(crate) fn preferred(&self, group: ToolGroup, smart_mosaic: bool) -> Tool {
        self.0[group.index()]
            .filter(|tool| smart_mosaic || *tool != Tool::SmartMosaic)
            .unwrap_or(group.members()[0])
    }
    /// 当前工具属于该组时直接显示它，否则显示最近使用项。
    pub(crate) fn shown(&self, group: ToolGroup, current: Tool, smart_mosaic: bool) -> Tool {
        if group.options(smart_mosaic).any(|tool| tool == current) {
            current
        } else {
            self.preferred(group, smart_mosaic)
        }
    }
}

/// 数字键作用于槽位：普通工具再按回到鼠标；分组从默认项起依次轮换，
/// 最后一项之后回到鼠标，并把首项恢复为默认，下一轮从头开始。
pub(crate) fn cycle(
    slot: Slot,
    current: Tool,
    memory: &mut GroupMemory,
    smart_mosaic: bool,
    mouse: Tool,
) -> Tool {
    match slot {
        Slot::Tool(tool) if tool == current => mouse,
        Slot::Tool(tool) => tool,
        Slot::Group(group) => {
            let options = group.options(smart_mosaic).collect::<Vec<_>>();
            match options.iter().position(|tool| *tool == current) {
                Some(index) if index + 1 == options.len() => {
                    memory.remember(options[0]);
                    mouse
                }
                Some(index) => options[index + 1],
                None => memory.preferred(group, smart_mosaic),
            }
        }
    }
}

/// 主栏上带下拉的按钮：绘制工具组与识别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flyout {
    Tools(ToolGroup),
    Recognize,
}

/// 识别下拉的各项；主按钮执行本次会话最近使用的一项（默认文字识别）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RecognizeMode {
    #[default]
    Text,
    /// 识别后打开面板“译文”页并翻译全文。
    TextTranslate,
    /// 识别后打开“表格”页；可信候选不足时放宽规则强制成表。
    Table,
    /// 仅将当前选区发送到已配置的 API，返回可编辑的 LaTeX 与本机排版预览。
    Formula,
    /// 译文按原位置覆盖到图上。
    ImageTranslate,
    Qr,
}
pub(crate) const RECOGNIZE_MODES: [RecognizeMode; 6] = [
    RecognizeMode::Text,
    RecognizeMode::TextTranslate,
    RecognizeMode::Table,
    RecognizeMode::Formula,
    RecognizeMode::ImageTranslate,
    RecognizeMode::Qr,
];
impl RecognizeMode {
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Text => "recognize-text",
            Self::TextTranslate => "recognize-translate",
            Self::Table => "recognize-table",
            Self::Formula => "recognize-formula",
            Self::ImageTranslate => "recognize-image-translate",
            Self::Qr => "recognize-qr",
        }
    }
    pub(crate) fn label(self) -> Text {
        match self {
            Self::Text => Text("文字识别", "Recognize text"),
            Self::TextTranslate => Text("识别并翻译", "Recognize & translate"),
            Self::Table => Text("表格识别", "Recognize table"),
            Self::Formula => Text("公式识别", "Recognize formula"),
            Self::ImageTranslate => Text("图片翻译", "Translate image"),
            Self::Qr => Text("二维码识别", "Scan QR code"),
        }
    }
    pub(crate) fn hint(self) -> Text {
        match self {
            Self::Text => Text("本机文字识别 · R", "Local text recognition · R"),
            Self::TextTranslate => Text(
                "识别并翻译 · T · 在面板“译文”页对照查看",
                "Recognize & translate · T · compare in the Translation tab",
            ),
            Self::Table => Text(
                "表格识别 · 按表格整理并复制到 Excel",
                "Recognize table · copy as a table to Excel",
            ),
            Self::Formula => Text(
                "公式识别 · API · 预览并复制 LaTeX",
                "Recognize formula · API · preview and copy LaTeX",
            ),
            Self::ImageTranslate => Text(
                "图片翻译 · Y · 译文覆盖到原文位置",
                "Translate image · Y · overlay translations on the text",
            ),
            Self::Qr => Text("二维码识别", "Scan QR code"),
        }
    }
    pub(crate) fn key(self) -> Option<&'static str> {
        match self {
            Self::Text => Some("R"),
            Self::TextTranslate => Some("T"),
            Self::ImageTranslate => Some("Y"),
            _ => None,
        }
    }
    /// 需要普通原位 OCR 结果宿主（截图或图片编辑）的项；贴图不提供。
    pub(crate) fn needs_text_host(self) -> bool {
        matches!(
            self,
            Self::TextTranslate | Self::Table | Self::Formula | Self::ImageTranslate
        )
    }
}

/// 悬停展开状态机：进入按钮短暂停留后展开，离开按钮与下拉一段时间后收起；
/// 点击箭头展开的下拉固定显示，直到再次点击、选择或点击空白处。
/// 每次进出都会让旧定时器失效，定时器触发时只按最新悬停状态求值。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FlyoutHover {
    open: Option<Flyout>,
    pinned: bool,
    pending: Option<Flyout>,
    over_root: Option<Flyout>,
    over_panel: bool,
    generation: u64,
}
impl FlyoutHover {
    pub(crate) fn open(&self) -> Option<Flyout> {
        self.open
    }
    pub(crate) fn pinned(&self) -> bool {
        self.open.is_some() && self.pinned
    }
    fn timer(&mut self) -> (u64, Duration) {
        self.generation = self.generation.wrapping_add(1);
        let delay = if self.pending.is_some() {
            HOVER_OPEN_DELAY
        } else {
            HOVER_CLOSE_DELAY
        };
        (self.generation, delay)
    }
    /// `allowed` 为 false（拖动、绘制或已有其它弹层）时只记录悬停，不排队展开。
    pub(crate) fn enter_root(&mut self, flyout: Flyout, allowed: bool) -> Option<(u64, Duration)> {
        self.over_root = Some(flyout);
        if self.open == Some(flyout) {
            return None;
        }
        if !allowed || (self.pinned && self.open.is_some()) {
            return None;
        }
        self.pending = Some(flyout);
        Some(self.timer())
    }
    pub(crate) fn leave_root(&mut self, flyout: Flyout) -> Option<(u64, Duration)> {
        if self.over_root == Some(flyout) {
            self.over_root = None;
        }
        if self.pending == Some(flyout) {
            self.pending = None;
        }
        (self.open.is_some() && !self.pinned).then(|| self.timer())
    }
    pub(crate) fn enter_panel(&mut self) {
        self.over_panel = true;
    }
    pub(crate) fn leave_panel(&mut self) -> Option<(u64, Duration)> {
        self.over_panel = false;
        (self.open.is_some() && !self.pinned).then(|| self.timer())
    }
    /// 返回是否需要重绘。
    pub(crate) fn fire(&mut self, generation: u64) -> bool {
        if generation != self.generation {
            return false;
        }
        if let Some(pending) = self.pending.take() {
            if self.over_root == Some(pending) {
                self.open = Some(pending);
                self.pinned = false;
                self.over_panel = false;
                return true;
            }
        }
        if self.open.is_some() && !self.pinned && self.over_root != self.open && !self.over_panel {
            self.open = None;
            return true;
        }
        false
    }
    /// 点击下拉箭头：悬停展开的下拉转为固定；已固定的再次点击收起。
    pub(crate) fn click(&mut self, flyout: Flyout) {
        self.pending = None;
        self.generation = self.generation.wrapping_add(1);
        if self.open == Some(flyout) && self.pinned {
            self.open = None;
            self.pinned = false;
        } else {
            self.open = Some(flyout);
            self.pinned = true;
        }
    }
    /// 返回之前是否有展开的下拉。
    pub(crate) fn close(&mut self) -> bool {
        self.pending = None;
        self.over_panel = false;
        self.pinned = false;
        self.generation = self.generation.wrapping_add(1);
        self.open.take().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolbar_config::{AVAILABLE_TOOLS, DEFAULT_TOOLS};

    #[test]
    fn toolbar_groups_merge_family_at_first_member_and_keep_ungrouped_order() {
        assert_eq!(
            slots(&DEFAULT_TOOLS, true),
            vec![
                Slot::Group(ToolGroup::Shape),
                Slot::Group(ToolGroup::Line),
                Slot::Group(ToolGroup::Mark),
                Slot::Group(ToolGroup::Text),
                Slot::Group(ToolGroup::Mosaic),
                Slot::Group(ToolGroup::Eraser),
            ]
        );
        assert_eq!(
            slots(&DEFAULT_TOOLS, false),
            DEFAULT_TOOLS.map(Slot::Tool).to_vec()
        );
        // 组位置跟随首个组员，重复与固定入口被忽略。
        assert_eq!(
            slots(
                &[
                    Tool::Blur,
                    Tool::Crop,
                    Tool::Arrow,
                    Tool::Mosaic,
                    Tool::Ellipse
                ],
                true
            ),
            vec![
                Slot::Group(ToolGroup::Mosaic),
                Slot::Group(ToolGroup::Line),
                Slot::Group(ToolGroup::Shape),
            ]
        );
        let all = slots(&AVAILABLE_TOOLS, true);
        // 六组加独立的套索裁剪。
        assert_eq!(all.len(), 7);
        for tool in AVAILABLE_TOOLS {
            assert_eq!(all.iter().filter(|slot| slot.contains(tool)).count(), 1);
        }
        assert!(slots(&[], true).is_empty());
    }

    #[test]
    fn toolbar_groups_every_member_belongs_to_one_group_and_smart_mosaic_is_capture_only() {
        for group in GROUPS {
            for tool in group.members() {
                assert_eq!(group_of(*tool), Some(group));
            }
        }
        assert_eq!(group_of(Tool::Arrow), Some(ToolGroup::Line));
        assert_eq!(group_of(Tool::Serial), Some(ToolGroup::Text));
        assert_eq!(group_of(Tool::Spotlight), Some(ToolGroup::Mark));
        assert_eq!(group_of(Tool::Lasso), None);
        assert_eq!(group_of(Tool::Crop), None);
        assert!(ToolGroup::Mosaic
            .options(true)
            .any(|tool| tool == Tool::SmartMosaic));
        assert!(!ToolGroup::Mosaic
            .options(false)
            .any(|tool| tool == Tool::SmartMosaic));
        let mut memory = GroupMemory::default();
        memory.remember(Tool::SmartMosaic);
        assert_eq!(memory.preferred(ToolGroup::Mosaic, true), Tool::SmartMosaic);
        assert_eq!(memory.preferred(ToolGroup::Mosaic, false), Tool::Mosaic);
    }

    #[test]
    fn toolbar_shortcut_numbers_follow_slots_and_cycle_back_to_mouse() {
        assert_eq!(shortcut_number(0), Some(1));
        assert_eq!(shortcut_number(8), Some(9));
        assert_eq!(shortcut_number(9), None);
        assert_eq!(shortcut_digit("7"), Some(7));
        assert_eq!(shortcut_digit("0"), Some(0));
        assert_eq!(shortcut_digit("f1"), None);
        assert_eq!(shortcut_digit("10"), None);

        let mut memory = GroupMemory::default();
        let shape = Slot::Group(ToolGroup::Shape);
        let mut tool = Tool::Edit;
        let mut seen = Vec::new();
        for _ in 0..7 {
            tool = cycle(shape, tool, &mut memory, true, Tool::Edit);
            seen.push(tool);
        }
        assert_eq!(
            seen,
            [
                Tool::Rectangle,
                Tool::Ellipse,
                Tool::Triangle,
                Tool::Diamond,
                Tool::Star,
                Tool::Edit,
                Tool::Rectangle
            ]
        );
        // 从最近使用项开始；一轮结束后默认项回到首项。
        memory.remember(Tool::Ellipse);
        assert_eq!(
            cycle(shape, Tool::Arrow, &mut memory, true, Tool::Edit),
            Tool::Ellipse
        );
        assert_eq!(
            cycle(shape, Tool::Star, &mut memory, true, Tool::Edit),
            Tool::Edit
        );
        assert_eq!(memory.preferred(ToolGroup::Shape, true), Tool::Rectangle);
        let lasso = Slot::Tool(Tool::Lasso);
        assert_eq!(
            cycle(lasso, Tool::Pen, &mut memory, true, Tool::Edit),
            Tool::Lasso
        );
        assert_eq!(
            cycle(lasso, Tool::Lasso, &mut memory, true, Tool::Edit),
            Tool::Edit
        );
        // 图片编辑没有智能马赛克，打码组在局部填充后回到鼠标。
        let mosaic = Slot::Group(ToolGroup::Mosaic);
        assert_eq!(
            cycle(mosaic, Tool::LocalFill, &mut memory, false, Tool::Edit),
            Tool::Edit
        );
        assert_eq!(
            cycle(mosaic, Tool::LocalFill, &mut memory, true, Tool::Edit),
            Tool::SmartMosaic
        );
    }

    #[test]
    fn toolbar_flyout_hover_opens_after_dwell_and_survives_crossing_into_panel() {
        let shape = Flyout::Tools(ToolGroup::Shape);
        let mark = Flyout::Tools(ToolGroup::Mark);
        let mut hover = FlyoutHover::default();
        // 快速掠过：离开后旧定时器失效。
        let (stale, delay) = hover.enter_root(shape, true).unwrap();
        assert_eq!(delay, HOVER_OPEN_DELAY);
        assert!(hover.leave_root(shape).is_none());
        assert!(!hover.fire(stale));
        assert_eq!(hover.open(), None);

        let (generation, _) = hover.enter_root(shape, true).unwrap();
        assert!(hover.fire(generation));
        assert_eq!(hover.open(), Some(shape));
        // 从按钮移入下拉：中间的离开不收起。
        let (generation, delay) = hover.leave_root(shape).unwrap();
        assert_eq!(delay, HOVER_CLOSE_DELAY);
        hover.enter_panel();
        assert!(!hover.fire(generation));
        assert_eq!(hover.open(), Some(shape));
        // 沿主栏移到另一组：替换为新组。
        let close = hover.leave_panel().unwrap();
        let (generation, _) = hover.enter_root(mark, true).unwrap();
        assert!(!hover.fire(close.0));
        assert!(hover.fire(generation));
        assert_eq!(hover.open(), Some(mark));
        // 移到空白处后收起。
        let (generation, _) = hover.leave_root(mark).unwrap();
        assert!(hover.fire(generation));
        assert_eq!(hover.open(), None);
        // 不允许时只记录悬停。
        assert!(hover.enter_root(shape, false).is_none());
        assert_eq!(hover.open(), None);
    }

    #[test]
    fn toolbar_flyout_click_pins_hover_and_pinned_ignores_leave() {
        let shape = Flyout::Tools(ToolGroup::Shape);
        let mut hover = FlyoutHover::default();
        let (generation, _) = hover.enter_root(shape, true).unwrap();
        assert!(hover.fire(generation));
        // 悬停展开后点击箭头：转为固定，不因离开而收起。
        hover.click(shape);
        assert_eq!(hover.open(), Some(shape));
        assert!(hover.leave_root(shape).is_none());
        assert!(hover
            .enter_root(Flyout::Tools(ToolGroup::Mark), true)
            .is_none());
        assert_eq!(hover.open(), Some(shape));
        hover.click(shape);
        assert_eq!(hover.open(), None);
        hover.click(Flyout::Tools(ToolGroup::Mark));
        assert!(hover.close());
        assert!(!hover.close());
    }

    #[test]
    fn toolbar_recognize_modes_have_unique_ids_and_only_text_and_image_keys() {
        let ids: std::collections::BTreeSet<_> =
            RECOGNIZE_MODES.iter().map(|mode| mode.id()).collect();
        assert_eq!(ids.len(), RECOGNIZE_MODES.len());
        assert_eq!(RecognizeMode::default(), RecognizeMode::Text);
        let keys: Vec<_> = RECOGNIZE_MODES.iter().filter_map(|m| m.key()).collect();
        assert_eq!(keys, ["R", "T", "Y"]);
        assert!(!RecognizeMode::Text.needs_text_host());
        assert!(RecognizeMode::Table.needs_text_host());
    }
}
