//! 画布文字的结束合同；单行属性框自行确认，水印副栏 VM 路径另行适配。
use gpui::Modifiers;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextKeyAction {
    /// 原生输入法拥有组合中的按键，不能提交对象或转到宿主快捷键。
    Ime,
    Consume,
    Finish,
    LineBreak,
    DeleteEmpty,
    Pass,
}

/// 提交/结束后按住原键不能继续复制、收栏或关闭宿主。
pub(crate) fn terminal_held(key: &str, is_held: bool) -> bool {
    is_held && matches!(key, "enter" | "escape")
}

pub(crate) fn decide(
    key: &str,
    modifiers: Modifiers,
    is_held: bool,
    marked: bool,
    text_empty: bool,
) -> TextKeyAction {
    // 系统/应用修饰键不属于此文字合同，Alt+F4 等仍由原生窗口处理。
    if modifiers.alt || modifiers.platform {
        return TextKeyAction::Pass;
    }
    if marked {
        return TextKeyAction::Ime;
    }
    if terminal_held(key, is_held) {
        return TextKeyAction::Consume;
    }
    match key {
        "enter" if modifiers.shift => TextKeyAction::LineBreak,
        "enter" | "escape" => TextKeyAction::Finish,
        "backspace" | "delete" if text_empty && is_held => TextKeyAction::Consume,
        "backspace" | "delete" if text_empty => TextKeyAction::DeleteEmpty,
        _ => TextKeyAction::Pass,
    }
}
