//! OCR 行选择与临时筛选状态。原始识别文字和几何不在此处改写。

use std::{collections::BTreeSet, ops::Range};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SelectionMode {
    Replace,
    Add,
    Subtract,
    Toggle,
}

impl SelectionMode {
    /// 修饰键由宿主在按下鼠标时保存；Ctrl 点击切换，Ctrl 拖框追加。
    pub(crate) fn for_gesture(control: bool, shift: bool, alt: bool, dragged: bool) -> Self {
        if alt {
            Self::Subtract
        } else if control && !dragged {
            Self::Toggle
        } else if control || shift {
            Self::Add
        } else {
            Self::Replace
        }
    }
}

#[derive(Clone, Debug)]
struct ItemChange {
    index: usize,
    excluded: bool,
    restored: bool,
}

#[derive(Clone, Debug)]
struct Edit {
    items: Vec<ItemChange>,
    selection: BTreeSet<usize>,
}

/// 每次识别结果变化时重新创建。自动判断与用户排除、恢复各自保留。
#[derive(Clone, Debug)]
pub(crate) struct ResultSelection {
    pub(crate) selected: BTreeSet<usize>,
    pub(crate) auto_filter: bool,
    automatic: Vec<bool>,
    excluded: BTreeSet<usize>,
    restored: BTreeSet<usize>,
    edits: Vec<Edit>,
}

impl ResultSelection {
    pub(crate) fn new(automatic: Vec<bool>) -> Self {
        Self {
            selected: BTreeSet::new(),
            auto_filter: false,
            automatic,
            excluded: BTreeSet::new(),
            restored: BTreeSet::new(),
            edits: Vec::new(),
        }
    }

    pub(crate) fn is_hidden(&self, index: usize) -> bool {
        self.automatic.get(index).is_some_and(|automatic| {
            self.excluded.contains(&index)
                || (self.auto_filter && *automatic && !self.restored.contains(&index))
        })
    }

    pub(crate) fn is_manually_excluded(&self, index: usize) -> bool {
        index < self.automatic.len() && self.excluded.contains(&index)
    }

    pub(crate) fn hidden_indices(&self) -> Vec<usize> {
        (0..self.automatic.len())
            .filter(|&index| self.is_hidden(index))
            .collect()
    }

    pub(crate) fn has_hidden(&self) -> bool {
        (0..self.automatic.len()).any(|index| self.is_hidden(index))
    }

    pub(crate) fn kept_count(&self) -> usize {
        (0..self.automatic.len())
            .filter(|&index| !self.is_hidden(index))
            .count()
    }

    pub(crate) fn selected_count(&self) -> usize {
        self.selected
            .iter()
            .filter(|&&index| self.is_selectable(index))
            .count()
    }

    pub(crate) fn set_auto_filter(&mut self, enabled: bool) -> bool {
        if self.auto_filter == enabled {
            return false;
        }
        self.auto_filter = enabled;
        self.retain_visible_selection();
        true
    }

    /// 每次拖动都从手势开始时的选择计算，缩回框时不会残留上一帧命中。
    pub(crate) fn apply_hits(
        &mut self,
        base: &BTreeSet<usize>,
        hits: impl IntoIterator<Item = usize>,
        mode: SelectionMode,
    ) {
        let hits: BTreeSet<_> = hits
            .into_iter()
            .filter(|&index| self.is_selectable(index))
            .collect();
        let base: BTreeSet<_> = base
            .iter()
            .copied()
            .filter(|&index| self.is_selectable(index))
            .collect();
        self.selected = match mode {
            SelectionMode::Replace => hits,
            SelectionMode::Add => base.union(&hits).copied().collect(),
            SelectionMode::Subtract => base.difference(&hits).copied().collect(),
            SelectionMode::Toggle => base.symmetric_difference(&hits).copied().collect(),
        };
    }

    /// 取消手势时传回开始快照；隐藏项与不存在的序号始终不能重新选中。
    pub(crate) fn replace_selection(&mut self, indices: impl IntoIterator<Item = usize>) {
        self.selected = indices
            .into_iter()
            .filter(|&index| self.is_selectable(index))
            .collect();
    }

    pub(crate) fn exclude_selected(&mut self) -> bool {
        let items: Vec<_> = self
            .selected
            .iter()
            .copied()
            .filter(|&index| self.is_selectable(index))
            .map(|index| self.item_change(index))
            .collect();
        if items.is_empty() {
            return false;
        }
        for item in &items {
            self.excluded.insert(item.index);
            self.restored.remove(&item.index);
        }
        self.edits.push(Edit {
            items,
            selection: self.selected.clone(),
        });
        self.retain_visible_selection();
        true
    }

    /// 恢复的自动候选在本次结果中豁免，之后开关自动过滤也不会再次隐藏。
    pub(crate) fn restore(&mut self, indices: impl IntoIterator<Item = usize>) -> bool {
        let indices: BTreeSet<_> = indices
            .into_iter()
            .filter(|&index| self.is_hidden(index))
            .collect();
        let items: Vec<_> = indices
            .into_iter()
            .map(|index| self.item_change(index))
            .collect();
        if items.is_empty() {
            return false;
        }
        for item in &items {
            self.excluded.remove(&item.index);
            if self.automatic[item.index] {
                self.restored.insert(item.index);
            }
        }
        self.edits.push(Edit {
            items,
            selection: self.selected.clone(),
        });
        true
    }

    pub(crate) fn can_undo(&self) -> bool {
        !self.edits.is_empty()
    }

    /// 一次排除或恢复对应一个增量；自动开关不进入该历史。
    pub(crate) fn undo(&mut self) -> bool {
        let Some(edit) = self.edits.pop() else {
            return false;
        };
        for item in edit.items {
            set_membership(&mut self.excluded, item.index, item.excluded);
            set_membership(&mut self.restored, item.index, item.restored);
        }
        self.replace_selection(edit.selection);
        true
    }

    /// 按原识别行顺序复制，不会夹带两条不连续选中行之间的正文。
    /// 非法范围被忽略，正常行的空白保持原样。
    pub(crate) fn copy_text(
        &self,
        source: &str,
        ranges: &[Range<usize>],
        selected_only: bool,
    ) -> String {
        ranges
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                self.is_selectable(*index) && (!selected_only || self.selected.contains(index))
            })
            .filter_map(|(_, range)| source.get(range.clone()))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn is_selectable(&self, index: usize) -> bool {
        index < self.automatic.len() && !self.is_hidden(index)
    }

    fn retain_visible_selection(&mut self) {
        let selected = std::mem::take(&mut self.selected);
        self.replace_selection(selected);
    }

    fn item_change(&self, index: usize) -> ItemChange {
        ItemChange {
            index,
            excluded: self.excluded.contains(&index),
            restored: self.restored.contains(&index),
        }
    }
}

fn set_membership(set: &mut BTreeSet<usize>, index: usize, present: bool) {
    if present {
        set.insert(index);
    } else {
        set.remove(&index);
    }
}

#[cfg(test)]
#[path = "ocr_result_selection_tests.rs"]
mod tests;
