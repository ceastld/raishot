//! 水印副栏的纯输入状态：文档是内容来源，IME 预编辑不进入历史。
use crate::document::ObjectId;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

const HISTORY_WINDOW: Duration = Duration::from_secs(2);
static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

/// 绑定版本只用于拒绝迟到的控件事件，不持有文档或图像。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BindingToken {
    instance: u64,
    generation: u64,
    target: Option<ObjectId>,
}

/// 与滚轮分组分开使用的 typed 令牌；宿主不能将其当作普通原始 u64 分组。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct HistoryToken {
    instance: u64,
    target: ObjectId,
    binding_generation: u64,
    sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EditKind {
    Create,
    Replace,
}

/// 先准备、再写 Document、最后 acknowledge；准备失败不能消费已有分组。
#[derive(Clone, Debug)]
pub(crate) struct PreparedEdit {
    pub(crate) kind: EditKind,
    pub(crate) text: Option<String>,
    pub(crate) history: Option<HistoryToken>,
    binding: BindingToken,
    version: u64,
    at: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EditError {
    StaleBinding,
    StalePrepared,
    MissingCreatedTarget,
    UnexpectedCreatedTarget,
}
impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::StaleBinding => "水印输入绑定已失效",
            Self::StalePrepared => "水印输入准备结果已失效",
            Self::MissingCreatedTarget => "新水印尚未绑定到已提交对象",
            Self::UnexpectedCreatedTarget => "水印替换不能改绑另一个对象",
        })
    }
}
impl std::error::Error for EditError {}

#[derive(Debug)]
pub(crate) struct WatermarkEdit {
    instance: u64,
    target: Option<ObjectId>,
    generation: u64,
    version: u64,
    sequence: u64,
    committed: String,
    preview: Option<String>,
    history: Option<HistoryToken>,
    last_change: Option<Duration>,
}
impl Default for WatermarkEdit {
    fn default() -> Self {
        Self {
            instance: NEXT_INSTANCE
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("水印输入实例计数耗尽"),
            target: None,
            generation: 0,
            version: 0,
            sequence: 0,
            committed: String::new(),
            preview: None,
            history: None,
            last_change: None,
        }
    }
}
impl WatermarkEdit {
    /// 程序同步、Undo/Redo 和重新选工具均静默重载，不反向生成文档命令。
    pub(crate) fn bind(&mut self, target: Option<ObjectId>, text: &str) -> BindingToken {
        self.generation = next_counter(self.generation);
        self.version = next_counter(self.version);
        self.target = target;
        // 旧文档可能含多行，加载只读取原始值；用户确认输入时才应用单行规则。
        self.committed = text.to_owned();
        self.preview = None;
        self.history = None;
        self.last_change = None;
        self.binding()
    }

    pub(crate) fn binding(&self) -> BindingToken {
        BindingToken {
            instance: self.instance,
            generation: self.generation,
            target: self.target,
        }
    }

    pub(crate) fn target(&self) -> Option<ObjectId> {
        self.target
    }

    pub(crate) fn committed_text(&self) -> &str {
        &self.committed
    }

    pub(crate) fn display_text(&self) -> &str {
        self.preview.as_deref().unwrap_or(&self.committed)
    }

    pub(crate) fn has_preview(&self) -> bool {
        self.preview.is_some()
    }

    pub(crate) fn set_preview(
        &mut self,
        binding: BindingToken,
        text: &str,
    ) -> Result<(), EditError> {
        self.check_binding(binding)?;
        self.preview = Some(normalize_single_line(text));
        Ok(())
    }

    /// 包括确认结果与 committed 相同的情况；仅结束 IME，不改内容和历史。
    pub(crate) fn cancel_preview(&mut self, binding: BindingToken) -> Result<(), EditError> {
        self.check_binding(binding)?;
        self.preview = None;
        Ok(())
    }

    pub(crate) fn prepare_text(
        &self,
        binding: BindingToken,
        text: &str,
        now: Duration,
    ) -> Result<Option<PreparedEdit>, EditError> {
        self.check_binding(binding)?;
        let text = normalize_single_line(text);
        if text == self.committed || (self.target.is_none() && text.trim().is_empty()) {
            return Ok(None);
        }
        Ok(Some(self.prepare(Some(text), now)))
    }

    pub(crate) fn prepare_style(
        &self,
        binding: BindingToken,
        changed: bool,
        now: Duration,
    ) -> Result<Option<PreparedEdit>, EditError> {
        self.check_binding(binding)?;
        if !changed || self.target.is_none() {
            return Ok(None);
        }
        Ok(Some(self.prepare(None, now)))
    }

    fn prepare(&self, text: Option<String>, now: Duration) -> PreparedEdit {
        let kind = if self.target.is_some() {
            EditKind::Replace
        } else {
            EditKind::Create
        };
        let history = self.target.map(|target| {
            let continuous = self
                .last_change
                .and_then(|last| now.checked_sub(last))
                .is_some_and(|elapsed| elapsed <= HISTORY_WINDOW);
            if continuous {
                if let Some(history) = self.history {
                    return history;
                }
            }
            HistoryToken {
                instance: self.instance,
                target,
                binding_generation: self.generation,
                sequence: next_counter(self.sequence),
            }
        });
        PreparedEdit {
            kind,
            text,
            history,
            binding: self.binding(),
            version: self.version,
            at: now,
        }
    }

    /// 宿主应在写 Document 前检查；同步写入和 acknowledge 之间不得让出控制权。
    pub(crate) fn accepts(&self, prepared: &PreparedEdit) -> bool {
        prepared.binding == self.binding() && prepared.version == self.version
    }

    pub(crate) fn acknowledge(
        &mut self,
        prepared: PreparedEdit,
        created_target: Option<ObjectId>,
    ) -> Result<BindingToken, EditError> {
        self.check_binding(prepared.binding)?;
        if prepared.version != self.version {
            return Err(EditError::StalePrepared);
        }
        // 所有可失败的检查都在修改状态前完成。
        let target = match prepared.kind {
            EditKind::Create => Some(created_target.ok_or(EditError::MissingCreatedTarget)?),
            EditKind::Replace => {
                if created_target.is_some() {
                    return Err(EditError::UnexpectedCreatedTarget);
                }
                self.target
            }
        };
        if let Some(text) = prepared.text {
            self.committed = text;
        }
        self.target = target;
        self.preview = None;
        self.version = next_counter(self.version);
        if prepared.kind == EditKind::Create {
            // Insert 与之后第一次 Replace 必须是两次 Undo。
            self.generation = next_counter(self.generation);
            self.history = None;
            self.last_change = None;
        } else {
            self.history = prepared.history;
            self.last_change = Some(prepared.at);
            if let Some(history) = prepared.history {
                self.sequence = history.sequence;
            }
        }
        Ok(self.binding())
    }

    /// 文档其它命令断开合并，也使命令前的准备结果失效；不主动取消 IME。
    pub(crate) fn break_history(&mut self) {
        self.version = next_counter(self.version);
        self.history = None;
        self.last_change = None;
    }

    fn check_binding(&self, binding: BindingToken) -> Result<(), EditError> {
        if binding == self.binding() {
            Ok(())
        } else {
            Err(EditError::StaleBinding)
        }
    }
}

fn next_counter(value: u64) -> u64 {
    // 一次宿主生命期内不复用令牌，避免 saturating/wrapping 将旧事件复活。
    value.checked_add(1).expect("水印输入令牌计数耗尽")
}

/// 保留中文和已有空格；CRLF 是一个换行，粘贴换行变成单行分隔空格。
pub(crate) fn normalize_single_line(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                result.push(' ');
            }
            '\n' | '\t' | '\u{2028}' | '\u{2029}' => result.push(' '),
            ch if !ch.is_control() => result.push(ch),
            _ => {}
        }
    }
    result
}

/// 旧多水印只选择一个稳定目标；列表按文档层序传入，最后一项是最上层。
pub(crate) fn choose_target(
    selected: Option<ObjectId>,
    managed: Option<ObjectId>,
    watermark_ids: &[ObjectId],
) -> Option<ObjectId> {
    selected
        .filter(|id| watermark_ids.contains(id))
        .or_else(|| managed.filter(|id| watermark_ids.contains(id)))
        .or_else(|| watermark_ids.last().copied())
}
