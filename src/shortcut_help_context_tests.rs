use super::*;
use crate::toolbar_config::ToolbarConfig;

#[test]
fn help_drawing_keys_use_existing_context_letters_without_static_digit_assignments() {
    let capture = Context::Capture { smart_mosaic: true };
    let capture_bindings = drawing_bindings(capture);
    let pin_bindings = drawing_bindings(Context::Pin);
    for tool in std::iter::once(Tool::Edit).chain(AVAILABLE_TOOLS) {
        for (context, bindings) in [(capture, &capture_bindings), (Context::Pin, &pin_bindings)] {
            assert_eq!(
                bindings
                    .iter()
                    .find_map(|(listed, key)| (*listed == tool).then_some(*key)),
                shortcut_hints::letter(tool, context),
                "帮助必须读取现有上下文字母，不维护第二张键表"
            );
        }
    }
    assert!(!pin_bindings
        .iter()
        .any(|(_, key)| matches!(*key, "O" | "S")));
    let original = ToolbarConfig::default();
    let reordered = ToolbarConfig {
        tools: vec![Tool::Mosaic, Tool::Text],
        grouped: true,
    };
    assert_ne!(
        shortcut_hints::for_tool(Tool::Mosaic, Context::Pin, &original).digit,
        shortcut_hints::for_tool(Tool::Mosaic, Context::Pin, &reordered).digit,
        "真正数字随排序改变，帮助不得固定映射"
    );
    assert_eq!(
        drawing_bindings(Context::Pin),
        pin_bindings,
        "隐藏和重排按钮不改变字母帮助"
    );
    let text = help_text("测试版本", "Alt+F9", "已禁用");
    assert!(text.contains("测试版本") && text.contains("Alt+F9") && text.contains("已禁用"));
    let capture_section = text
        .split_once("\n截图\n")
        .expect("完整帮助缺少截图部分")
        .1
        .split_once("\n贴图\n")
        .expect("完整帮助缺少贴图部分")
        .0;
    let pin_section = text
        .split_once("\n贴图\n")
        .expect("完整帮助缺少贴图部分")
        .1
        .split_once("\n长截图（")
        .expect("完整帮助缺少长截图部分")
        .0;
    for (section, bindings) in [
        (capture_section, &capture_bindings),
        (pin_section, &pin_bindings),
    ] {
        for (tool, key) in bindings {
            let displayed_binding = format!("{key} {}", tool.label());
            assert!(
                section.contains(&displayed_binding),
                "完整帮助必须在对应上下文插入真实工具字母：{displayed_binding}"
            );
        }
    }
    for clause in [
        "Enter / ✓  复制长图并结束",
        "F9  开始／停止",
        "F10 / 空格  暂停／继续",
        "成片在主界面“历史”页",
    ] {
        assert!(text.contains(clause), "完整帮助缺少工作流说明：{clause}");
    }
    assert!(!text.contains("Ctrl+A 切换当前/全部屏幕"));
}

#[test]
fn ocr_selection_exit_hint_avoids_eraser_letter_when_drawing_tools_own_it() {
    assert_eq!(
        shortcut_hints::letter(Tool::PixelEraser, Context::Pin),
        Some("E")
    );
    let hint = pin_ocr_selection_hint();
    assert!(hint.contains("Esc"));
    assert!(
        !hint.contains(" E "),
        "OCR完成状态不能把改选工具的E当退出入口"
    );
    assert!(hint.contains("菜单") && hint.contains("输入"));
    let help = help_text("测试版本", "Alt+F9", "已禁用");
    assert!(help.contains(
        "绘制栏收起或贴图锁定时：
E  选择文字"
    ));
    assert!(help.contains("绘制栏展开时按字母选工具"));
}
