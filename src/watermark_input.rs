//! 水印副栏的输入元数据；组合预览与已确认正文分开，不持有图片或标注文档。
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatermarkFinishKey {
    Enter,
    Escape,
}

/// 宿主只把 Committed 写入永久历史；Preview 可用于临时组合文字显示。
pub(crate) enum WatermarkInputEvent {
    Preview { text: String, composing: bool },
    Committed { text: String },
    Finish { key: WatermarkFinishKey },
    FocusChanged { focused: bool },
}

struct CompositionCheckpoint {
    selection: Range<usize>,
    reversed: bool,
}

pub(crate) struct WatermarkInputState {
    committed: String,
    checkpoint: Option<CompositionCheckpoint>,
    focused: bool,
}

impl WatermarkInputState {
    pub(crate) fn new(text: &str) -> Self {
        Self {
            committed: text.to_owned(),
            checkpoint: None,
            focused: false,
        }
    }

    pub(crate) fn committed_text(&self) -> &str {
        &self.committed
    }

    pub(crate) fn is_composing(&self) -> bool {
        self.checkpoint.is_some()
    }

    pub(crate) fn begin_composition(&mut self, selection: Range<usize>, reversed: bool) {
        // 后续候选更新仍恢复第一次组合前的正文及选区。
        if self.checkpoint.is_none() {
            self.checkpoint = Some(CompositionCheckpoint {
                selection,
                reversed,
            });
        }
    }

    pub(crate) fn cancel_composition(&mut self) -> Option<(String, Range<usize>, bool)> {
        self.checkpoint.take().map(|checkpoint| {
            (
                self.committed.clone(),
                checkpoint.selection,
                checkpoint.reversed,
            )
        })
    }

    pub(crate) fn confirm(&mut self, text: &str) -> bool {
        self.checkpoint = None;
        if self.committed == text {
            return false;
        }
        self.committed.clear();
        self.committed.push_str(text);
        true
    }

    pub(crate) fn set_text(&mut self, text: &str) {
        self.checkpoint = None;
        self.committed.clear();
        self.committed.push_str(text);
    }

    pub(crate) fn focus_changed(&mut self, focused: bool) -> bool {
        if self.focused == focused {
            return false;
        }
        self.focused = focused;
        true
    }
}

/// 换行与 Tab 留一个空格，CRLF 只留一个；按 Unicode 标量限制内容长度。
pub(crate) fn single_line(text: &str) -> String {
    let mut result = String::new();
    let mut previous_cr = false;
    let mut count = 0;
    for character in text.chars() {
        let replacement = match character {
            '\n' if previous_cr => {
                previous_cr = false;
                continue;
            }
            '\r' | '\n' | '\t' | '\u{0085}' | '\u{2028}' | '\u{2029}' => ' ',
            value if value.is_control() => {
                previous_cr = false;
                continue;
            }
            value => value,
        };
        if count == 4096 {
            break;
        }
        result.push(replacement);
        count += 1;
        previous_cr = character == '\r';
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermark_input_single_line_keeps_word_boundaries_and_limit() {
        assert_eq!(
            single_line("中文\r\n水印\n第二行\t尾部\0"),
            "中文 水印 第二行 尾部"
        );
        let long = "字".repeat(4097);
        assert_eq!(single_line(&long).chars().count(), 4096);
    }

    #[test]
    fn watermark_input_cancellation_restores_original_selection_and_text() {
        let mut state = WatermarkInputState::new("甲乙丙");
        state.begin_composition(3..6, true);
        state.begin_composition(3..9, false);
        assert_eq!(state.committed_text(), "甲乙丙");
        assert_eq!(
            state.cancel_composition(),
            Some(("甲乙丙".into(), 3..6, true))
        );
        assert!(!state.is_composing());
        assert!(state.cancel_composition().is_none());
    }

    #[test]
    fn watermark_input_confirmation_and_program_sync_are_distinct() {
        let mut state = WatermarkInputState::new("旧字");
        state.begin_composition(0..6, false);
        assert_eq!(state.committed_text(), "旧字");
        assert!(state.confirm("新字"));
        assert!(!state.confirm("新字"));
        state.begin_composition(0..6, false);
        state.set_text("撤销值");
        assert!(!state.is_composing());
        assert_eq!(state.committed_text(), "撤销值");
        assert!(state.cancel_composition().is_none());
    }

    #[test]
    fn watermark_input_focus_is_idempotent_and_not_content() {
        let mut state = WatermarkInputState::new("水印");
        assert!(state.focus_changed(true));
        assert!(!state.focus_changed(true));
        assert!(state.focus_changed(false));
        assert!(!state.focus_changed(false));
        assert!(!state.confirm("水印"));
    }
}
