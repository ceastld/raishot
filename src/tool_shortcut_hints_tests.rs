//! 只核提示与既有纯路由的合同；不创建窗口，也不模拟宿主按键事件。
use super::{for_tool, group_tooltip, tooltip, Context, Shortcut};
use crate::{
    annotation::Tool,
    pin_keys::toolbar_shortcuts::{self, DigitContext, DigitRoute},
    toolbar_config::{ToolbarConfig, AVAILABLE_TOOLS},
    toolbar_groups::{self, GroupMemory, Slot, ToolGroup},
};

#[test]
fn tool_shortcut_hint_letters_cover_routes_even_when_toolbar_tools_are_hidden() {
    let hidden = ToolbarConfig {
        tools: Vec::new(),
        grouped: true,
    };
    let expected = [
        (Tool::Crop, None, None),
        (Tool::Edit, Some("V"), Some("V")),
        (Tool::Rectangle, None, None),
        (Tool::Ellipse, None, None),
        (Tool::Triangle, None, None),
        (Tool::Diamond, None, None),
        (Tool::Star, None, None),
        (Tool::Arrow, None, None),
        (Tool::Pen, None, None),
        (Tool::Mosaic, Some("M"), Some("M")),
        (Tool::Highlighter, Some("H"), Some("H")),
        (Tool::HighlighterRect, None, None),
        (Tool::Polyline, Some("P"), Some("P")),
        (Tool::Text, Some("X"), Some("X")),
        (Tool::Watermark, None, None),
        (Tool::Serial, Some("N"), Some("N")),
        (Tool::Blur, Some("B"), Some("B")),
        (Tool::MosaicBrush, None, None),
        (Tool::BlurBrush, None, None),
        (Tool::LocalFill, None, None),
        (Tool::Spotlight, Some("S"), None),
        (Tool::Eraser, Some("Shift+E"), Some("Shift+E")),
        (Tool::PixelEraser, Some("E"), Some("E")),
        (Tool::Lasso, Some("O"), None),
        (Tool::SmartMosaic, None, None),
    ];
    assert_eq!(expected.len(), AVAILABLE_TOOLS.len() + 3);
    for (tool, capture_letter, pin_letter) in expected {
        for smart_mosaic in [false, true] {
            let shortcut = for_tool(tool, Context::Capture { smart_mosaic }, &hidden);
            assert_eq!(shortcut.letter, capture_letter, "截图字母键：{tool:?}");
            assert_eq!(
                shortcut.digit,
                (tool == Tool::Edit).then_some(0),
                "隐藏工具不能获得不存在的数字槽位：{tool:?}"
            );
            assert_eq!(shortcut.cycles, tool == Tool::Edit);
        }
        let shortcut = for_tool(tool, Context::Pin, &hidden);
        assert_eq!(shortcut.letter, pin_letter, "贴图字母键：{tool:?}");
        assert_eq!(shortcut.digit, (tool == Tool::Edit).then_some(0));
        assert!(!shortcut.cycles, "贴图平铺工具不应提示组内轮换");
    }
}

#[test]
fn tool_shortcut_hint_capture_digits_follow_reordered_groups_and_real_cycle_members() {
    let config = ToolbarConfig {
        tools: vec![
            Tool::Blur,
            Tool::Arrow,
            Tool::Highlighter,
            Tool::Text,
            Tool::Mosaic,
            Tool::Pen,
            Tool::Crop,
            Tool::SmartMosaic,
            Tool::Edit,
            Tool::Blur,
        ],
        grouped: true,
    };
    let slots = config.slots();
    assert_eq!(
        slots,
        [
            Slot::Group(ToolGroup::Mosaic),
            Slot::Group(ToolGroup::Line),
            Slot::Group(ToolGroup::Mark),
            Slot::Group(ToolGroup::Text),
        ]
    );
    for smart_mosaic in [false, true] {
        let context = Context::Capture { smart_mosaic };
        for (index, slot) in slots.iter().copied().enumerate() {
            let members = match slot {
                Slot::Tool(tool) => vec![tool],
                Slot::Group(group) => group.options(smart_mosaic).collect::<Vec<_>>(),
            };
            let digit = toolbar_groups::shortcut_number(index);
            let mut memory = GroupMemory::default();
            let mut current = Tool::Edit;
            for expected in members {
                current =
                    toolbar_groups::cycle(slot, current, &mut memory, smart_mosaic, Tool::Edit);
                assert_eq!(current, expected, "使用实际组成员顺序，非配置平铺顺序");
                let shortcut = for_tool(current, context, &config);
                assert_eq!(shortcut.digit, digit, "组内成员共享真实槽位");
                assert_eq!(shortcut.cycles, matches!(slot, Slot::Group(_)));
            }
            assert_eq!(
                toolbar_groups::cycle(slot, current, &mut memory, smart_mosaic, Tool::Edit),
                Tool::Edit,
                "一轮结束仍遵循原鼠标模式路由"
            );
        }
        let smart = for_tool(Tool::SmartMosaic, context, &config);
        assert_eq!(smart.digit, smart_mosaic.then_some(1));
        assert_eq!(smart.cycles, smart_mosaic);
    }
    assert_eq!(
        for_tool(
            Tool::Highlighter,
            Context::Capture {
                smart_mosaic: false
            },
            &config
        )
        .digit,
        Some(3)
    );
    let separate = ToolbarConfig {
        tools: AVAILABLE_TOOLS.iter().rev().copied().collect(),
        grouped: false,
    };
    for (index, tool) in separate.normalized().tools.into_iter().enumerate() {
        let shortcut = for_tool(tool, Context::Capture { smart_mosaic: true }, &separate);
        assert_eq!(shortcut.digit, toolbar_groups::shortcut_number(index));
        assert!(!shortcut.cycles, "独立按钮不能借同类分组产生轮换提示");
    }
    assert_eq!(
        for_tool(
            Tool::Rectangle,
            Context::Capture { smart_mosaic: true },
            &separate
        )
        .digit,
        None,
        "第十个之后的按钮没有数字快捷键"
    );
}

#[test]
fn tool_shortcut_hint_pin_digits_match_flat_normalized_route_and_fixed_edit_zero() {
    let mut tools = vec![Tool::Crop, Tool::Edit, Tool::SmartMosaic, Tool::Lasso];
    tools.extend(AVAILABLE_TOOLS.iter().rev().copied());
    tools.extend([Tool::Lasso, Tool::Text]);
    for grouped in [false, true] {
        let config = ToolbarConfig {
            tools: tools.clone(),
            grouped,
        };
        let visible = toolbar_shortcuts::visible_tools(&config);
        assert_eq!(visible.len(), AVAILABLE_TOOLS.len());
        assert_eq!(visible[0], Tool::Lasso);
        for (index, tool) in visible.into_iter().enumerate() {
            let shortcut = for_tool(tool, Context::Pin, &config);
            assert_eq!(shortcut.digit, toolbar_shortcuts::number(index));
            assert!(!shortcut.cycles, "贴图 grouped 设置不改变平铺路由");
            if let Some(digit) = shortcut.digit {
                for (current, expected) in [(Tool::Edit, tool), (tool, Tool::Edit)] {
                    assert_eq!(
                        toolbar_shortcuts::route(
                            &digit.to_string(),
                            Default::default(),
                            false,
                            DigitContext::Toolbar { editable: true },
                            &config,
                            current,
                        ),
                        DigitRoute::Select(expected),
                        "提示数字必须选中对应按钮，再按回到编辑"
                    );
                }
            }
        }
        assert_eq!(for_tool(Tool::Edit, Context::Pin, &config).digit, Some(0));
        for current in AVAILABLE_TOOLS {
            assert_eq!(
                toolbar_shortcuts::route(
                    "0",
                    Default::default(),
                    false,
                    DigitContext::Toolbar { editable: true },
                    &config,
                    current,
                ),
                DigitRoute::Select(Tool::Edit)
            );
        }
        assert_eq!(
            for_tool(Tool::SmartMosaic, Context::Pin, &config).digit,
            None
        );
    }
    let hidden_blur = ToolbarConfig {
        tools: vec![Tool::Text],
        grouped: true,
    };
    assert_eq!(
        for_tool(Tool::Blur, Context::Pin, &hidden_blur),
        Shortcut {
            letter: Some("B"),
            digit: None,
            cycles: false,
        }
    );
}

#[test]
fn tool_shortcut_hint_tooltips_preserve_gestures_and_name_group_letters_as_group_keys() {
    // 单元测试进程由 i18n 自身固定中文；这里不修改全局语言或用户设置。
    let label = "模糊；滚轮调节大小";
    assert_eq!(
        tooltip(
            label,
            Shortcut {
                letter: None,
                digit: None,
                cycles: false,
            },
        ),
        label
    );
    let single = tooltip(
        label,
        Shortcut {
            letter: Some("B"),
            digit: Some(3),
            cycles: false,
        },
    );
    assert!(single.starts_with(label), "保留按钮原有手势说明");
    assert!(single.contains('B') && single.contains('3'));
    assert!(!single.contains("轮换"), "独立按钮不冒充工具组");
    let cycle = tooltip(
        label,
        Shortcut {
            letter: Some("B"),
            digit: Some(3),
            cycles: true,
        },
    );
    assert!(cycle.starts_with(label));
    assert!(cycle.contains('B') && cycle.contains('3') && cycle.contains("轮换"));
    let config = ToolbarConfig {
        tools: vec![Tool::Pen, Tool::Blur],
        grouped: true,
    };
    let context = Context::Capture {
        smart_mosaic: false,
    };
    let group = group_tooltip("打码", ToolGroup::Mosaic, Tool::LocalFill, context, &config);
    assert!(group.starts_with("打码"));
    assert!(group.contains('2') && group.contains("轮换"));
    assert!(group.contains("组内") && group.contains('M') && group.contains('B'));
    let pen_group = group_tooltip("画笔", ToolGroup::Mark, Tool::Pen, context, &config);
    assert!(pen_group.contains("组内") && pen_group.contains('H'));
    assert_eq!(for_tool(Tool::Pen, context, &config).letter, None);
    assert_eq!(
        for_tool(Tool::Highlighter, context, &config).letter,
        Some("H")
    );
}
