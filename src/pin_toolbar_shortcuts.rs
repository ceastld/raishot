//! 贴图平铺工具栏的显示顺序与数字键共用模型；原生输入由宿主先行识别。
use crate::{
    annotation::Tool,
    toolbar_config::{normalized_tools, ToolbarConfig},
    toolbar_groups::{self, GroupMemory, Slot},
};

/// 贴图当前平铺显示每个工具；配置 grouped 不会隐式改变它的数字槽位。
pub(crate) fn visible_tools(config: &ToolbarConfig) -> Vec<Tool> {
    normalized_tools(&config.tools)
}

pub(crate) fn number(index: usize) -> Option<u8> {
    // 共享实现的 then_some 会提前求值；越界先拒绝，避免极大 index 转换后加一溢出。
    if index >= 9 {
        return None;
    }
    toolbar_groups::shortcut_number(index)
}

/// 移动模式虽然保留 editor.tool，数字键仍须与未激活的绘制按钮保持一致。
pub(crate) fn current_tool(tool: Tool, editing: bool) -> Tool {
    if editing {
        tool
    } else {
        Tool::Edit
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DigitContext {
    Collapsed,
    NativeInput,
    Toolbar { editable: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DigitRoute {
    Pass,
    Consume,
    Select(Tool),
}

pub(crate) fn route(
    key: &str,
    modifiers: gpui::Modifiers,
    is_held: bool,
    context: DigitContext,
    config: &ToolbarConfig,
    current: Tool,
) -> DigitRoute {
    let DigitContext::Toolbar { editable } = context else {
        return DigitRoute::Pass;
    };
    if modifiers.shift
        || modifiers.control
        || modifiers.alt
        || modifiers.platform
        || modifiers.function
    {
        return DigitRoute::Pass;
    }
    let Some(digit) = toolbar_groups::shortcut_digit(key) else {
        return DigitRoute::Pass;
    };
    if is_held || !editable {
        return DigitRoute::Consume;
    }
    if digit == 0 {
        return DigitRoute::Select(Tool::Edit);
    }
    let tools = visible_tools(config);
    let Some(&tool) = tools.get(usize::from(digit - 1)) else {
        return DigitRoute::Consume;
    };
    let mut memory = GroupMemory::default();
    DigitRoute::Select(toolbar_groups::cycle(
        Slot::Tool(tool),
        current,
        &mut memory,
        false,
        Tool::Edit,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toolbar_config::AVAILABLE_TOOLS;

    fn toolbar() -> DigitContext {
        DigitContext::Toolbar { editable: true }
    }

    fn plain_route(key: &str, config: &ToolbarConfig, current: Tool) -> DigitRoute {
        route(key, Default::default(), false, toolbar(), config, current)
    }

    #[test]
    fn pin_toolbar_digit_default_hints_match_visible_tool_order() {
        let config = ToolbarConfig::default();
        let expected = [
            Tool::Rectangle,
            Tool::Arrow,
            Tool::Pen,
            Tool::Text,
            Tool::Ellipse,
            Tool::Mosaic,
            Tool::PixelEraser,
        ];
        assert_eq!(visible_tools(&config), expected);
        for (index, tool) in expected.into_iter().enumerate() {
            let digit = index as u8 + 1;
            assert_eq!(number(index), Some(digit));
            assert_eq!(
                plain_route(&digit.to_string(), &config, Tool::Edit),
                DigitRoute::Select(tool),
                "数字提示与当前可见按钮必须同序"
            );
        }
    }

    #[test]
    fn pin_toolbar_digit_hidden_reordered_tools_stay_flat_and_normalized() {
        let mut config = ToolbarConfig {
            tools: vec![
                Tool::Mosaic,
                Tool::Crop,
                Tool::Rectangle,
                Tool::Ellipse,
                Tool::Mosaic,
                Tool::Edit,
                Tool::SmartMosaic,
                Tool::Pen,
            ],
            grouped: true,
        };
        let expected = [Tool::Mosaic, Tool::Rectangle, Tool::Ellipse, Tool::Pen];
        assert_eq!(visible_tools(&config), expected);
        config.grouped = false;
        assert_eq!(visible_tools(&config), expected, "贴图平铺不依截图分组槽");
        for grouped in [false, true] {
            config.grouped = grouped;
            for (index, tool) in expected.into_iter().enumerate() {
                assert_eq!(
                    plain_route(&(index + 1).to_string(), &config, Tool::Edit),
                    DigitRoute::Select(tool)
                );
            }
            assert_eq!(plain_route("5", &config, Tool::Text), DigitRoute::Consume);
            assert!(!visible_tools(&config).contains(&Tool::Arrow));
            assert!(!visible_tools(&config).contains(&Tool::Text));
        }
    }

    #[test]
    fn pin_toolbar_digit_empty_or_short_list_consumes_unmapped_numbers() {
        for count in [0, 1, 8, 9] {
            let config = ToolbarConfig {
                tools: AVAILABLE_TOOLS[..count].to_vec(),
                grouped: true,
            };
            assert_eq!(visible_tools(&config).len(), count);
            for digit in 1u8..=9 {
                let expected = config
                    .tools
                    .get(usize::from(digit - 1))
                    .copied()
                    .map_or(DigitRoute::Consume, DigitRoute::Select);
                assert_eq!(
                    plain_route(&digit.to_string(), &config, Tool::Edit),
                    expected,
                    "空列表或未映射的数字不能落回其它贴图动作"
                );
            }
        }
    }

    #[test]
    fn pin_toolbar_digit_more_than_nine_tools_only_numbers_first_nine() {
        let config = ToolbarConfig {
            tools: AVAILABLE_TOOLS.to_vec(),
            grouped: true,
        };
        let visible = visible_tools(&config);
        assert!(visible.len() > 9, "超过九项仍应保留所有可见工具");
        for (index, tool) in visible.into_iter().enumerate() {
            if index < 9 {
                let digit = index as u8 + 1;
                assert_eq!(number(index), Some(digit));
                assert_eq!(
                    plain_route(&digit.to_string(), &config, Tool::Edit),
                    DigitRoute::Select(tool)
                );
            } else {
                assert_eq!(number(index), None);
            }
        }
        assert_eq!(number(usize::MAX), None);
        assert_eq!(number(255), None);
        assert_eq!(plain_route("10", &config, Tool::Edit), DigitRoute::Pass);
    }

    #[test]
    fn pin_toolbar_digit_held_consumes_and_fresh_press_cycles_back_to_edit() {
        let config = ToolbarConfig::default();
        assert_eq!(
            plain_route("1", &config, Tool::Edit),
            DigitRoute::Select(Tool::Rectangle)
        );
        for key in ["1", "0"] {
            assert_eq!(
                route(
                    key,
                    Default::default(),
                    true,
                    toolbar(),
                    &config,
                    Tool::Rectangle,
                ),
                DigitRoute::Consume,
                "长按不重复切换，也不提前回到编辑"
            );
        }
        assert_eq!(
            plain_route("1", &config, Tool::Rectangle),
            DigitRoute::Select(Tool::Edit)
        );
        assert_eq!(
            plain_route("1", &config, Tool::Edit),
            DigitRoute::Select(Tool::Rectangle),
            "抬起后重新按下允许开始下一轮"
        );
    }

    #[test]
    fn pin_toolbar_digit_move_mode_starts_visible_tool_before_fresh_press_can_cycle() {
        let config = ToolbarConfig::default();
        let retained = Tool::Rectangle;
        assert_eq!(current_tool(retained, false), Tool::Edit);
        assert_eq!(
            plain_route("1", &config, current_tool(retained, false)),
            DigitRoute::Select(Tool::Rectangle),
            "移动模式首次按旧工具数字应激活绘制，而不是切回编辑"
        );
        assert_eq!(current_tool(retained, true), Tool::Rectangle);
        assert_eq!(
            plain_route("1", &config, current_tool(retained, true)),
            DigitRoute::Select(Tool::Edit),
            "已激活绘制后重新按下才能切回编辑"
        );
        for editing in [false, true] {
            assert_eq!(
                plain_route("0", &config, current_tool(retained, editing)),
                DigitRoute::Select(Tool::Edit)
            );
        }
        for retained in AVAILABLE_TOOLS {
            assert_eq!(current_tool(retained, false), Tool::Edit);
            assert_eq!(current_tool(retained, true), retained);
            for (index, visible) in visible_tools(&config).into_iter().enumerate() {
                assert_eq!(
                    plain_route(
                        &(index + 1).to_string(),
                        &config,
                        current_tool(retained, false),
                    ),
                    DigitRoute::Select(visible),
                    "移动模式保留的任意工具不能污染可见数字槽位"
                );
            }
        }
    }

    #[test]
    fn pin_toolbar_digit_noneditable_toolbar_consumes_without_selection() {
        let config = ToolbarConfig::default();
        for is_held in [false, true] {
            for digit in 0u8..=9 {
                assert_eq!(
                    route(
                        &digit.to_string(),
                        Default::default(),
                        is_held,
                        DigitContext::Toolbar { editable: false },
                        &config,
                        Tool::Text,
                    ),
                    DigitRoute::Consume
                );
            }
        }
    }

    #[test]
    fn pin_toolbar_digit_zero_always_selects_edit_instead_of_text_or_image_action() {
        let config = ToolbarConfig {
            tools: vec![Tool::Text, Tool::Watermark],
            grouped: true,
        };
        for current in [
            Tool::Crop,
            Tool::Edit,
            Tool::Text,
            Tool::Watermark,
            Tool::Pen,
        ] {
            assert_eq!(
                plain_route("0", &config, current),
                DigitRoute::Select(Tool::Edit)
            );
        }
        assert_eq!(
            plain_route("1", &config, Tool::Edit),
            DigitRoute::Select(Tool::Text)
        );
    }

    #[test]
    fn pin_toolbar_digit_collapsed_passes_to_existing_host_route() {
        let config = ToolbarConfig::default();
        for is_held in [false, true] {
            for digit in 0u8..=9 {
                assert_eq!(
                    route(
                        &digit.to_string(),
                        Default::default(),
                        is_held,
                        DigitContext::Collapsed,
                        &config,
                        Tool::Text,
                    ),
                    DigitRoute::Pass,
                    "折叠状态不替换既有宿主数字键合同"
                );
            }
        }
    }

    #[test]
    fn pin_toolbar_digit_native_input_has_priority_for_plain_and_modified_keys() {
        let config = ToolbarConfig::default();
        for modifiers in [
            gpui::Modifiers::default(),
            gpui::Modifiers {
                shift: true,
                control: true,
                alt: true,
                platform: true,
                function: true,
            },
        ] {
            for is_held in [false, true] {
                for key in ["0", "1", "9", "a", "", "１２"] {
                    assert_eq!(
                        route(
                            key,
                            modifiers,
                            is_held,
                            DigitContext::NativeInput,
                            &config,
                            Tool::Text,
                        ),
                        DigitRoute::Pass,
                        "原生文字输入不能被数字工具选择消费"
                    );
                }
            }
        }
    }

    #[test]
    fn pin_toolbar_digit_only_single_ascii_without_any_modifier_is_selected() {
        let config = ToolbarConfig::default();
        for modifiers in [
            gpui::Modifiers {
                shift: true,
                ..Default::default()
            },
            gpui::Modifiers {
                control: true,
                ..Default::default()
            },
            gpui::Modifiers {
                alt: true,
                ..Default::default()
            },
            gpui::Modifiers {
                platform: true,
                ..Default::default()
            },
            gpui::Modifiers {
                function: true,
                ..Default::default()
            },
        ] {
            for key in ["0", "1", "9"] {
                assert_eq!(
                    route(key, modifiers, false, toolbar(), &config, Tool::Edit),
                    DigitRoute::Pass,
                    "含修饰数字保留上层路由，不参与工具切换"
                );
            }
        }
        for key in ["", "00", "10", "１", "٠", "a", " 1", "1 ", "numpad1"] {
            assert_eq!(plain_route(key, &config, Tool::Edit), DigitRoute::Pass);
        }
    }
}
