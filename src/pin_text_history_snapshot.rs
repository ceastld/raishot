//! 文字显示映射的 typed 适配；外部记录须先校验原始语义，映射不能代替解码校验。
use super::{
    BoundedHistorySnapshot, Change, Document, HistoryBudgetExceeded, MAX_HISTORY_COMMANDS,
    MAX_HISTORY_DEPTH, MAX_HISTORY_NODES,
};
use crate::{annotation::Annotation, Result};

/// 两栈只从较长栈底去掉最远命令，当前对象与分配时钟始终保留。
fn trim_oldest(snapshot: &mut BoundedHistorySnapshot) -> bool {
    if !snapshot.undo.is_empty() && snapshot.undo.len() >= snapshot.redo.len() {
        snapshot.undo.remove(0);
        true
    } else if !snapshot.redo.is_empty() {
        snapshot.redo.remove(0);
        true
    } else {
        false
    }
}

fn map_change(
    mut change: Change,
    map: &impl Fn(&mut Annotation),
    depth: usize,
    nodes: &mut usize,
) -> Result<Option<Change>> {
    if depth > MAX_HISTORY_DEPTH {
        return Err(HistoryBudgetExceeded("文字历史映射嵌套深度超过上限").into());
    }
    match &mut change {
        Change::Insert { object, .. } | Change::Remove { object, .. } => {
            map(&mut object.annotation);
        }
        Change::Replace { before, after, .. } => {
            map(before);
            map(after);
        }
        Change::Clear(objects) => {
            for object in objects {
                map(&mut object.annotation);
            }
        }
        Change::Batch(changes) => {
            let mut kept = Vec::new();
            for child in std::mem::take(changes) {
                if let Some(child) = map_change(child, map, depth + 1, nodes)? {
                    kept.push(child);
                }
            }
            *changes = kept;
        }
        Change::LongClip { .. }
        | Change::CropMask { .. }
        | Change::Groups { .. }
        | Change::NestedGroups { .. }
        | Change::Order { .. } => {}
    }
    // 同一显示映射可能把原本不同的颜色或字体合并；等值节点不再占一步撤销。
    if Document::history_noop(&change) {
        return Ok(None);
    }
    *nodes += 1;
    if *nodes > MAX_HISTORY_NODES {
        return Err(HistoryBudgetExceeded("文字历史映射命令节点超过上限").into());
    }
    Ok(Some(change))
}

fn map_stack(
    changes: &[Change],
    map: &impl Fn(&mut Annotation),
    nodes: &mut usize,
) -> Result<Vec<Change>> {
    let mut kept = Vec::new();
    for change in changes {
        if let Some(change) = map_change(change.clone(), map, 0, nodes)? {
            kept.push(change);
        }
    }
    Ok(kept)
}

impl BoundedHistorySnapshot {
    /// 映射须确定且无外部副作用；仅用于已校验记录或真实内存文档的显示适配。
    /// 保留全部 Annotation 字段、对象身份与时钟；去掉映射后等值节点，再用原核心校验。
    pub(crate) fn map_annotations(
        mut self,
        map: impl Fn(&mut Annotation),
    ) -> Result<BoundedHistorySnapshot> {
        for object in &mut self.items {
            map(&mut object.annotation);
        }
        while self.undo.len().saturating_add(self.redo.len()) > MAX_HISTORY_COMMANDS {
            trim_oldest(&mut self);
        }
        loop {
            let mut mapped = self.clone();
            let mapped_result: Result<BoundedHistorySnapshot> = (|| {
                let mut nodes = 0;
                mapped.undo = map_stack(&self.undo, &map, &mut nodes)?;
                mapped.redo = map_stack(&self.redo, &map, &mut nodes)?;
                Document::from_bounded_history_snapshot(mapped.clone())?;
                Ok(mapped)
            })();
            match mapped_result {
                Ok(snapshot) => return Ok(snapshot),
                Err(error) if error.is::<HistoryBudgetExceeded>() => {
                    if !trim_oldest(&mut self) {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Document {
    /// 从真实两栈取得最近的平衡连续后缀；不改活动文档，不提交任何恢复命令。
    pub(crate) fn bounded_history_snapshot_mapped(
        &self,
        map: impl Fn(&mut Annotation),
    ) -> Result<BoundedHistorySnapshot> {
        self.validate_history_items()?;
        let mut undo_count = self.undo.len().min(MAX_HISTORY_COMMANDS);
        let mut redo_count = self.redo.len().min(MAX_HISTORY_COMMANDS);
        while undo_count + redo_count > MAX_HISTORY_COMMANDS {
            if undo_count >= redo_count {
                undo_count -= 1;
            } else {
                redo_count -= 1;
            }
        }
        BoundedHistorySnapshot {
            items: self.items.clone(),
            crop_mask: self.crop_mask.clone(),
            long_clip: self.long_clip.clone(),
            undo: self.undo[self.undo.len() - undo_count..].to_vec(),
            redo: self.redo[self.redo.len() - redo_count..].to_vec(),
            next_id: self.next_id,
            next_group: self.next_group,
        }
        .map_annotations(map)
    }
}
