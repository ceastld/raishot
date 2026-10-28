//! 历史页的空格预览决策；调用方负责焦点、打开窗口和录像播放。

use gpui::Modifiers;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HistorySource {
    Shot,
    Recording,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SpaceAction {
    ViewImage,
    ToggleRecording,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SpaceContext {
    pub source: HistorySource,
    /// 仅控制新图片预览；录像继续由宿主原有播放入口判断资格。
    pub image_preview_enabled: bool,
    /// 由宿主实际文本控件的焦点状态给出；搜索框也属于文本输入。
    pub text_input_focused: bool,
    pub composing: bool,
}

/// `None` 表示不消费按键。一次非重复 Space 请求打开查看器，关闭沿用其 Esc 行为。
/// 录像动作仅指向宿主现有的播放切换，不在这里创建另一套播放状态。
pub(crate) fn decide(
    key: &str,
    modifiers: Modifiers,
    is_held: bool,
    context: SpaceContext,
) -> Option<SpaceAction> {
    if key != "space"
        || is_held
        || modifiers.control
        || modifiers.alt
        || modifiers.shift
        || modifiers.platform
        || modifiers.function
        || context.text_input_focused
        || context.composing
    {
        return None;
    }
    match context.source {
        HistorySource::Shot if context.image_preview_enabled => Some(SpaceAction::ViewImage),
        HistorySource::Recording => Some(SpaceAction::ToggleRecording),
        HistorySource::Shot | HistorySource::Other => None,
    }
}

#[cfg(test)]
#[path = "image_quick_preview_tests.rs"]
mod tests;
