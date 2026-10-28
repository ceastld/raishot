//! 工具提示读取实际数字槽位；字母只描述既有路由，不修改按键行为。
use crate::{
    annotation::Tool,
    pin_keys::toolbar_shortcuts,
    toolbar_config::ToolbarConfig,
    toolbar_groups::{self, Slot, ToolGroup},
};

#[derive(Clone, Copy)]
pub(crate) enum Context {
    Capture { smart_mosaic: bool },
    Pin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shortcut {
    pub letter: Option<&'static str>,
    pub digit: Option<u8>,
    pub cycles: bool,
}

/// 截图与贴图标注状态的现有字母分支；隐藏按钮不移除字母路由。
pub(crate) fn letter(tool: Tool, context: Context) -> Option<&'static str> {
    Some(match tool {
        Tool::Edit => "V",
        Tool::Mosaic => "M",
        Tool::Highlighter => "H",
        Tool::Serial => "N",
        Tool::Text => "X",
        Tool::Blur => "B",
        Tool::Polyline => "P",
        Tool::PixelEraser => "E",
        Tool::Eraser => "Shift+E",
        Tool::Lasso if matches!(context, Context::Capture { .. }) => "O",
        Tool::Spotlight if matches!(context, Context::Capture { .. }) => "S",
        _ => return None,
    })
}

pub(crate) fn for_tool(tool: Tool, context: Context, config: &ToolbarConfig) -> Shortcut {
    let mut shortcut = Shortcut {
        letter: letter(tool, context),
        digit: None,
        cycles: false,
    };
    if tool == Tool::Edit {
        shortcut.digit = Some(0);
        shortcut.cycles = matches!(context, Context::Capture { .. });
        return shortcut;
    }
    match context {
        Context::Capture { smart_mosaic } => {
            if tool == Tool::SmartMosaic && !smart_mosaic {
                return shortcut;
            }
            if let Some((index, slot)) = config
                .slots()
                .into_iter()
                .enumerate()
                .find(|(_, slot)| slot.contains(tool))
                .filter(|(index, _)| *index < 9)
            {
                shortcut.digit = toolbar_groups::shortcut_number(index);
                shortcut.cycles = matches!(slot, Slot::Group(_));
            }
        }
        Context::Pin => {
            shortcut.digit = toolbar_shortcuts::visible_tools(config)
                .iter()
                .position(|visible| *visible == tool)
                .and_then(toolbar_shortcuts::number);
        }
    }
    shortcut
}

pub(crate) fn tooltip(label: &str, shortcut: Shortcut) -> String {
    match (shortcut.letter, shortcut.digit, shortcut.cycles) {
        (Some(key), Some(digit), true) => {
            trf!(
                "{label} · {key} · {digit} 轮换",
                "{label} · {key} · {digit} cycles"
            )
        }
        (Some(key), Some(digit), false) => format!("{label} · {key} / {digit}"),
        (Some(key), None, _) => format!("{label} · {key}"),
        (None, Some(digit), true) => {
            trf!("{label} · {digit} 轮换", "{label} · {digit} cycles")
        }
        (None, Some(digit), false) => format!("{label} · {digit}"),
        (None, None, _) => label.to_owned(),
    }
}

/// 组内字母须明确标为组成员，不能暗示它选择当前显示的铅笔、水印等工具。
pub(crate) fn group_tooltip(
    label: &str,
    group: ToolGroup,
    shown: Tool,
    context: Context,
    config: &ToolbarConfig,
) -> String {
    let smart_mosaic = matches!(context, Context::Capture { smart_mosaic: true });
    let keys = group
        .options(smart_mosaic)
        .filter_map(|tool| letter(tool, context))
        .collect::<Vec<_>>()
        .join(" / ");
    let group_label = group.label().get();
    let shortcut = for_tool(shown, context, config);
    let detail = match shortcut.digit.filter(|_| shortcut.cycles) {
        Some(digit) => trf!(
            "{group_label} · {digit} 轮换",
            "{group_label} · {digit} cycles"
        ),
        None => group_label.to_owned(),
    };
    if keys.is_empty() {
        trf!("{label}（{detail}）", "{label} ({detail})")
    } else {
        trf!(
            "{label}（{detail} · 组内 {keys}）",
            "{label} ({detail} · Group keys {keys})"
        )
    }
}

#[cfg(test)]
#[path = "tool_shortcut_hints_tests.rs"]
mod tests;
