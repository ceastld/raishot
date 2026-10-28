//! 对象文档与增量撤销命令。一次手势只提交一次；不保存位图快照。
use crate::annotation::Annotation;

#[path = "object_groups.rs"]
mod object_groups;
pub(crate) use object_groups::GroupMembership;

#[path = "document_long_clip.rs"]
mod long_clip;
#[path = "pin_text_history_snapshot.rs"]
mod text_history_snapshot;

pub type ObjectId = u64;
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditGroup {
    Wheel(u64),
    Watermark(crate::watermark_edit::HistoryToken),
}
#[derive(Clone, Debug, PartialEq)]
pub struct Object {
    pub id: ObjectId,
    pub annotation: Annotation,
    pub group: Option<u64>,
    pub nested_groups: Vec<u64>,
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Change {
    LongClip {
        before: Option<crate::long_clip::LongClip>,
        after: Option<crate::long_clip::LongClip>,
    },
    CropMask {
        before: Option<crate::crop_mask::Polygon>,
        after: Option<crate::crop_mask::Polygon>,
    },
    Insert {
        index: usize,
        object: Object,
    },
    Replace {
        index: usize,
        before: Annotation,
        after: Annotation,
    },
    Remove {
        index: usize,
        object: Object,
    },
    Clear(Vec<Object>),
    Batch(Vec<Change>),
    Groups {
        before: Vec<(ObjectId, Option<u64>)>,
        after: Vec<(ObjectId, Option<u64>)>,
    },
    NestedGroups {
        before: Vec<(ObjectId, GroupMembership)>,
        after: Vec<(ObjectId, GroupMembership)>,
    },
    Order {
        before: Vec<ObjectId>,
        after: Vec<ObjectId>,
    },
}

/// 保存与恢复使用真实增量命令，不另建一套应用算法。
pub(crate) type HistoryCommand = Change;

pub(crate) const MAX_HISTORY_COMMANDS: usize = 128;
pub(crate) const MAX_HISTORY_OBJECTS: usize = 1024;
const MAX_HISTORY_NODES: usize = 8192;
const MAX_HISTORY_PAYLOADS: usize = 32768;
const MAX_HISTORY_DEPTH: usize = 8;
const MAX_HISTORY_ID: u64 = u32::MAX as u64;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct BoundedHistorySnapshot {
    pub(crate) items: Vec<Object>,
    pub(crate) crop_mask: Option<crate::crop_mask::Polygon>,
    pub(crate) long_clip: Option<crate::long_clip::LongClip>,
    pub(crate) undo: Vec<HistoryCommand>,
    pub(crate) redo: Vec<HistoryCommand>,
    pub(crate) next_id: ObjectId,
    pub(crate) next_group: u64,
}

#[derive(Default)]
struct HistoryBudget {
    nodes: usize,
    payloads: usize,
    inserted: std::collections::BTreeSet<ObjectId>,
}

#[derive(Debug)]
struct HistoryBudgetExceeded(&'static str);
impl std::fmt::Display for HistoryBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for HistoryBudgetExceeded {}
#[derive(Default)]
pub struct Document {
    items: Vec<Object>,
    pub crop_mask: Option<crate::crop_mask::Polygon>,
    long_clip: Option<crate::long_clip::LongClip>,
    undo: Vec<Change>,
    redo: Vec<Change>,
    next_id: ObjectId,
    next_group: u64,
    revision: u64,
    edit_revision: u64,
    merge_group: Option<EditGroup>,
}
impl Document {
    /// 来源交接不裁两栈；真正超出既有128条/载荷容量时保留调用方原文档。
    pub(crate) fn exact_history_snapshot(&self) -> crate::Result<BoundedHistorySnapshot> {
        let snapshot = BoundedHistorySnapshot {
            items: self.items.clone(),
            crop_mask: self.crop_mask.clone(),
            long_clip: self.long_clip.clone(),
            undo: self.undo.clone(),
            redo: self.redo.clone(),
            next_id: self.next_id,
            next_group: self.next_group,
        };
        Self::from_bounded_history_snapshot(snapshot.clone())?;
        Ok(snapshot)
    }
    /// 两栈都按真实执行顺序保留连续后缀；当前对象及分配时钟不重编号。
    pub(crate) fn bounded_history_snapshot(&self) -> crate::Result<BoundedHistorySnapshot> {
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
        let mut snapshot = BoundedHistorySnapshot {
            items: self.items.clone(),
            crop_mask: self.crop_mask.clone(),
            long_clip: self.long_clip.clone(),
            undo: self.undo[self.undo.len() - undo_count..].to_vec(),
            redo: self.redo[self.redo.len() - redo_count..].to_vec(),
            next_id: self.next_id,
            next_group: self.next_group,
        };
        // 只裁数量／结构容量；命令不连续、ID错误等真实错误不能以裁剪掩盖。
        loop {
            match Self::from_bounded_history_snapshot(snapshot.clone()) {
                Ok(_) => return Ok(snapshot),
                Err(error) if error.is::<HistoryBudgetExceeded>() => {
                    if snapshot.undo.is_empty() && snapshot.redo.is_empty() {
                        return Err(error);
                    }
                    if snapshot.undo.len() >= snapshot.redo.len() {
                        snapshot.undo.remove(0);
                    } else {
                        snapshot.redo.remove(0);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// 完成结构、ID 与双向连续性验证后才交出新文档；恢复不提交任何编辑命令。
    pub(crate) fn from_bounded_history_snapshot(
        snapshot: BoundedHistorySnapshot,
    ) -> crate::Result<Self> {
        let document = Self {
            items: snapshot.items,
            crop_mask: snapshot.crop_mask,
            long_clip: snapshot.long_clip,
            undo: snapshot.undo,
            redo: snapshot.redo,
            next_id: snapshot.next_id,
            next_group: snapshot.next_group,
            ..Self::default()
        };
        document.validate_bounded_history()?;
        Ok(document)
    }

    fn history_probe(&self) -> Self {
        Self {
            items: self.items.clone(),
            crop_mask: self.crop_mask.clone(),
            long_clip: self.long_clip.clone(),
            next_id: self.next_id,
            next_group: self.next_group,
            ..Self::default()
        }
    }

    fn same_history_state(&self, other: &Self) -> bool {
        self.items == other.items
            && self.crop_mask == other.crop_mask
            && self.long_clip == other.long_clip
    }

    fn validate_bounded_history(&self) -> crate::Result<()> {
        self.validate_history_items()?;
        if self.undo.len().saturating_add(self.redo.len()) > MAX_HISTORY_COMMANDS {
            return Err(HistoryBudgetExceeded("文档撤销与重做命令超过128条").into());
        }
        let mut budget = HistoryBudget {
            payloads: self.items.len()
                + Self::long_clip_payloads(self.long_clip.as_ref())
                + self
                    .items
                    .iter()
                    .map(|object| object.nested_groups.len())
                    .sum::<usize>(),
            ..HistoryBudget::default()
        };
        for change in self.undo.iter().chain(&self.redo) {
            self.validate_history_shape(change, &mut budget, 0)?;
            if Self::history_noop(change) {
                return Err("文档历史包含没有变化的命令".into());
            }
        }
        let mut undo = self.history_probe();
        for change in self.undo.iter().rev() {
            undo.apply_history_checked(change, false)?;
        }
        for change in &self.undo {
            undo.apply_history_checked(change, true)?;
        }
        if !self.same_history_state(&undo) {
            return Err("文档撤销历史不能回到当前内容".into());
        }
        let mut redo = self.history_probe();
        for change in self.redo.iter().rev() {
            redo.apply_history_checked(change, true)?;
        }
        for change in &self.redo {
            redo.apply_history_checked(change, false)?;
        }
        if !self.same_history_state(&redo) {
            return Err("文档重做历史不能回到当前内容".into());
        }
        Ok(())
    }

    fn validate_history_id(&self, id: ObjectId) -> crate::Result<()> {
        if id == 0 || id > self.next_id {
            return Err("文档历史对象ID超出分配时钟".into());
        }
        Ok(())
    }

    fn validate_history_group(&self, group: Option<u64>) -> crate::Result<()> {
        if group.is_some_and(|id| id == 0 || id > self.next_group) {
            return Err("文档历史组ID超出分配时钟".into());
        }
        Ok(())
    }

    fn validate_history_objects(&self, objects: &[Object]) -> crate::Result<()> {
        if objects.len() > MAX_HISTORY_OBJECTS {
            return Err(HistoryBudgetExceeded("文档历史对象数量超过1024").into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for object in objects {
            self.validate_history_id(object.id)?;
            self.validate_history_group(object.group)?;
            object.membership().validate(self.next_group)?;
            if !ids.insert(object.id) {
                return Err("文档历史对象ID重复".into());
            }
        }
        Ok(())
    }

    fn validate_history_items(&self) -> crate::Result<()> {
        Self::validate_long_clip_pair(None, self.long_clip.as_ref())?;
        if self.next_id > MAX_HISTORY_ID || self.next_group > MAX_HISTORY_ID {
            return Err("文档历史分配时钟超过恢复上限".into());
        }
        self.validate_history_objects(&self.items)?;
        let inner_references = object_groups::validate_forest(&self.items, self.next_group)?;
        if self
            .items
            .len()
            .saturating_add(inner_references)
            .saturating_add(Self::long_clip_payloads(self.long_clip.as_ref()))
            > MAX_HISTORY_PAYLOADS
        {
            return Err(HistoryBudgetExceeded("文档组合内链引用超过上限").into());
        }
        Ok(())
    }

    fn validate_history_shape(
        &self,
        change: &Change,
        budget: &mut HistoryBudget,
        depth: usize,
    ) -> crate::Result<()> {
        budget.nodes += 1;
        if depth > MAX_HISTORY_DEPTH || budget.nodes > MAX_HISTORY_NODES {
            return Err(HistoryBudgetExceeded("文档历史命令节点或嵌套深度超过上限").into());
        }
        let payloads = match change {
            Change::LongClip { before, after } => {
                Self::validate_long_clip_pair(before.as_ref(), after.as_ref())?;
                Self::long_clip_payloads(before.as_ref())
                    .saturating_add(Self::long_clip_payloads(after.as_ref()))
            }
            Change::Batch(changes) => {
                if changes.is_empty() {
                    return Err("文档历史包含空批次".into());
                }
                for child in changes {
                    self.validate_history_shape(child, budget, depth + 1)?;
                }
                0
            }
            Change::Insert { index, object } | Change::Remove { index, object } => {
                if *index > MAX_HISTORY_OBJECTS {
                    return Err(HistoryBudgetExceeded("文档历史对象序位超出上限").into());
                }
                self.validate_history_id(object.id)?;
                self.validate_history_group(object.group)?;
                object.membership().validate(self.next_group)?;
                if matches!(change, Change::Insert { .. }) && !budget.inserted.insert(object.id) {
                    return Err("文档历史重复分配对象ID".into());
                }
                1 + object.nested_groups.len()
            }
            Change::Replace { index, .. } => {
                if *index >= MAX_HISTORY_OBJECTS {
                    return Err(HistoryBudgetExceeded("文档历史替换序位超出上限").into());
                }
                2
            }
            Change::Clear(objects) => {
                self.validate_history_objects(objects)?;
                objects.len()
                    + objects
                        .iter()
                        .map(|object| object.nested_groups.len())
                        .sum::<usize>()
            }
            Change::Groups { before, after } => {
                if before.len() > MAX_HISTORY_OBJECTS || after.len() > MAX_HISTORY_OBJECTS {
                    return Err(HistoryBudgetExceeded("文档历史分组载荷数量超出上限").into());
                }
                if before.is_empty() || before.len() != after.len() {
                    return Err("文档历史分组载荷数量无效".into());
                }
                let mut ids = std::collections::BTreeSet::new();
                for ((id, old), (new_id, new)) in before.iter().zip(after) {
                    self.validate_history_id(*id)?;
                    self.validate_history_group(*old)?;
                    self.validate_history_group(*new)?;
                    if id != new_id || !ids.insert(*id) {
                        return Err("文档历史分组对象ID不连续或重复".into());
                    }
                }
                before.len() + after.len()
            }
            Change::NestedGroups { before, after } => {
                if before.len() > MAX_HISTORY_OBJECTS || after.len() > MAX_HISTORY_OBJECTS {
                    return Err(HistoryBudgetExceeded("文档嵌套分组载荷数量超出上限").into());
                }
                if before.is_empty() || before.len() != after.len() {
                    return Err("文档嵌套分组载荷数量无效".into());
                }
                let mut ids = std::collections::BTreeSet::new();
                for ((id, old), (new_id, new)) in before.iter().zip(after) {
                    self.validate_history_id(*id)?;
                    old.validate(self.next_group)?;
                    new.validate(self.next_group)?;
                    if id != new_id || !ids.insert(*id) {
                        return Err("文档嵌套分组对象ID不连续或重复".into());
                    }
                }
                before.len()
                    + after.len()
                    + before
                        .iter()
                        .chain(after)
                        .map(|(_, membership)| membership.nested_groups.len())
                        .sum::<usize>()
            }
            Change::Order { before, after } => {
                if before.len() > MAX_HISTORY_OBJECTS || after.len() > MAX_HISTORY_OBJECTS {
                    return Err(HistoryBudgetExceeded("文档历史层序载荷数量超出上限").into());
                }
                if before.is_empty() || after.len() != before.len() {
                    return Err("文档历史层序载荷数量无效".into());
                }
                let old: std::collections::BTreeSet<_> = before.iter().copied().collect();
                let new: std::collections::BTreeSet<_> = after.iter().copied().collect();
                if old.len() != before.len() || new.len() != after.len() || old != new {
                    return Err("文档历史层序不是同组唯一对象ID".into());
                }
                for id in old {
                    self.validate_history_id(id)?;
                }
                before.len() + after.len()
            }
            Change::CropMask { .. } => 2,
        };
        budget.payloads += payloads;
        if budget.payloads > MAX_HISTORY_PAYLOADS {
            return Err(HistoryBudgetExceeded("文档历史载荷引用超过上限").into());
        }
        Ok(())
    }

    fn history_noop(change: &Change) -> bool {
        match change {
            Change::LongClip { before, after } => before == after,
            Change::CropMask { before, after } => before == after,
            Change::Replace { before, after, .. } => before == after,
            Change::Groups { before, after } => before == after,
            Change::NestedGroups { before, after } => before == after,
            Change::Order { before, after } => before == after,
            Change::Clear(objects) => objects.is_empty(),
            Change::Batch(changes) => changes.iter().all(Self::history_noop),
            Change::Insert { .. } | Change::Remove { .. } => false,
        }
    }

    /// 先在有界探针核全部子命令，再调用原 apply；不复制实际应用逻辑。
    fn apply_history_checked(&mut self, change: &Change, forward: bool) -> crate::Result<()> {
        let continuous =
            match change {
                Change::Batch(changes) => {
                    let mut probe = self.history_probe();
                    if forward {
                        for child in changes {
                            probe.apply_history_checked(child, true)?;
                        }
                    } else {
                        for child in changes.iter().rev() {
                            probe.apply_history_checked(child, false)?;
                        }
                    }
                    true
                }
                Change::CropMask { before, after } => {
                    &self.crop_mask == if forward { before } else { after }
                }
                Change::LongClip { before, after } => {
                    &self.long_clip == if forward { before } else { after }
                }
                Change::Insert { index, object } | Change::Remove { index, object } => {
                    let inserting = matches!(change, Change::Insert { .. }) == forward;
                    if inserting {
                        *index <= self.items.len()
                            && !self.items.iter().any(|existing| existing.id == object.id)
                    } else {
                        self.items.get(*index) == Some(object)
                    }
                }
                Change::Replace {
                    index,
                    before,
                    after,
                } => self.items.get(*index).is_some_and(|object| {
                    &object.annotation == if forward { before } else { after }
                }),
                Change::Clear(objects) => {
                    if forward {
                        &self.items == objects
                    } else {
                        self.items.is_empty()
                    }
                }
                Change::Groups { before, after } => (if forward { before } else { after })
                    .iter()
                    .all(|(id, group)| {
                        self.items.iter().any(|object| {
                            object.id == *id
                                && object.group == *group
                                && object.nested_groups.is_empty()
                        })
                    }),
                Change::NestedGroups { before, after } => (if forward { before } else { after })
                    .iter()
                    .all(|(id, membership)| {
                        self.items
                            .iter()
                            .any(|object| object.id == *id && object.membership() == *membership)
                    }),
                Change::Order { before, after } => self
                    .items
                    .iter()
                    .map(|object| object.id)
                    .eq((if forward { before } else { after }).iter().copied()),
            };
        if !continuous {
            return Err("文档历史命令与当前对象或遮罩不连续".into());
        }
        self.apply(change, forward);
        self.validate_history_items()
    }

    pub fn items(&self) -> &[Object] {
        &self.items
    }
    pub fn set_crop_mask(&mut self, after: Option<crate::crop_mask::Polygon>) {
        if self.crop_mask != after {
            self.commit(Change::CropMask {
                before: self.crop_mask.clone(),
                after,
            });
        }
    }
    pub fn invalidate_crop_mask(&mut self) {
        fn keep(change: Change) -> Option<Change> {
            match change {
                Change::CropMask { .. } => None,
                Change::Batch(items) => {
                    let items: Vec<_> = items.into_iter().filter_map(keep).collect();
                    (!items.is_empty()).then_some(Change::Batch(items))
                }
                other => Some(other),
            }
        }
        self.crop_mask = None;
        self.undo = std::mem::take(&mut self.undo)
            .into_iter()
            .filter_map(keep)
            .collect();
        self.redo = std::mem::take(&mut self.redo)
            .into_iter()
            .filter_map(keep)
            .collect();
        self.merge_group = None;
        self.revision += 1;
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// 只计新编辑，撤销/重做和无变化操作不增，供跨图像处理边界的重做失效判断。
    pub fn edit_revision(&self) -> u64 {
        self.edit_revision
    }
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
    pub fn get(&self, id: ObjectId) -> Option<&Annotation> {
        self.items
            .iter()
            .find(|o| o.id == id)
            .map(|o| &o.annotation)
    }
    pub fn insert(&mut self, annotation: Annotation) -> ObjectId {
        self.insert_at(annotation, self.items.len())
    }
    /// 托管水印在遮罩后、普通绘制前插入；历史仍保存同一对象和序位。
    pub(crate) fn insert_at(&mut self, annotation: Annotation, index: usize) -> ObjectId {
        self.next_id += 1;
        let id = self.next_id;
        self.commit(Change::Insert {
            index: index.min(self.items.len()),
            object: Object {
                id,
                annotation,
                group: None,
                nested_groups: Vec::new(),
            },
        });
        id
    }
    pub fn insert_many(&mut self, annotations: Vec<Annotation>) -> Vec<ObjectId> {
        let mut changes = Vec::new();
        let mut ids = Vec::new();
        for annotation in annotations {
            self.next_id += 1;
            let id = self.next_id;
            ids.push(id);
            changes.push(Change::Insert {
                index: self.items.len() + changes.len(),
                object: Object {
                    id,
                    annotation,
                    group: None,
                    nested_groups: Vec::new(),
                },
            });
        }
        if !changes.is_empty() {
            self.commit(Change::Batch(changes));
        }
        ids
    }
    /// 已准备的选字标记在一批历史内增补、改色或取消；不触碰相似的手工对象。
    pub(crate) fn insert_replace_remove_many(
        &mut self,
        insertions: Vec<Annotation>,
        replacements: &[(ObjectId, Annotation)],
        removals: &[ObjectId],
    ) -> Vec<ObjectId> {
        let mut changes: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, object)| {
                if removals.contains(&object.id) {
                    return None;
                }
                let (_, after) = replacements.iter().rev().find(|(id, _)| *id == object.id)?;
                (object.annotation != *after).then(|| Change::Replace {
                    index,
                    before: object.annotation.clone(),
                    after: after.clone(),
                })
            })
            .collect();
        let removed = self
            .items
            .iter()
            .filter(|object| removals.contains(&object.id))
            .count();
        changes.extend(
            self.items
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, object)| removals.contains(&object.id))
                .map(|(index, object)| Change::Remove {
                    index,
                    object: object.clone(),
                }),
        );
        let mut ids = Vec::with_capacity(insertions.len());
        for (offset, annotation) in insertions.into_iter().enumerate() {
            self.next_id += 1;
            ids.push(self.next_id);
            changes.push(Change::Insert {
                index: self.items.len() - removed + offset,
                object: Object {
                    id: self.next_id,
                    annotation,
                    group: None,
                    nested_groups: Vec::new(),
                },
            });
        }
        if !changes.is_empty() {
            self.commit(Change::Batch(changes));
        }
        ids
    }
    /// 新对象与已有对象的联动修改属于同一手势，撤销时一起恢复。
    pub fn insert_with_replacements(
        &mut self,
        annotation: Annotation,
        replacements: &[(ObjectId, Annotation)],
    ) -> ObjectId {
        self.insert_with_replacements_at(annotation, replacements, self.items.len())
    }
    pub(crate) fn insert_with_replacements_at(
        &mut self,
        annotation: Annotation,
        replacements: &[(ObjectId, Annotation)],
        index: usize,
    ) -> ObjectId {
        self.insert_with_replacements_in_scope_at(annotation, replacements, index, &[])
    }
    pub(crate) fn insert_with_replacements_in_scope_at(
        &mut self,
        annotation: Annotation,
        replacements: &[(ObjectId, Annotation)],
        index: usize,
        scope: &[u64],
    ) -> ObjectId {
        let membership = GroupMembership::from_path(&self.valid_group_scope(scope));
        let mut changes: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, object)| {
                let (_, after) = replacements.iter().rev().find(|(id, _)| *id == object.id)?;
                (object.annotation != *after).then(|| Change::Replace {
                    index,
                    before: object.annotation.clone(),
                    after: after.clone(),
                })
            })
            .collect();
        self.next_id += 1;
        let id = self.next_id;
        changes.push(Change::Insert {
            index: index.min(self.items.len()),
            object: Object {
                id,
                annotation,
                group: membership.group,
                nested_groups: membership.nested_groups,
            },
        });
        self.commit(Change::Batch(changes));
        id
    }
    pub fn replace(&mut self, id: ObjectId, after: Annotation) {
        if let Some(index) = self.items.iter().position(|o| o.id == id) {
            let before = self.items[index].annotation.clone();
            if before != after {
                self.commit(Change::Replace {
                    index,
                    before,
                    after,
                });
            }
        }
    }
    pub fn remove(&mut self, id: ObjectId) {
        if let Some(index) = self.items.iter().position(|o| o.id == id) {
            self.commit(Change::Remove {
                index,
                object: self.items[index].clone(),
            });
        }
    }
    pub fn clear(&mut self) {
        let mut changes = Vec::new();
        if !self.items.is_empty() {
            changes.push(Change::Clear(self.items.clone()));
        }
        if self.crop_mask.is_some() {
            changes.push(Change::CropMask {
                before: self.crop_mask.clone(),
                after: None,
            });
        }
        if !changes.is_empty() {
            self.commit(Change::Batch(changes));
        }
    }
    pub fn remove_many(&mut self, ids: &[ObjectId]) {
        let changes: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, o)| ids.contains(&o.id))
            .map(|(index, object)| Change::Remove {
                index,
                object: object.clone(),
            })
            .collect();
        if !changes.is_empty() {
            self.commit(Change::Batch(changes));
        }
    }
    pub fn replace_many(&mut self, objects: &[(ObjectId, Annotation)]) {
        self.replace_many_grouped(objects, None);
    }
    /// 复合对象整合一次提交；替换使用原序位，删除逆序执行，撤销恢复原层位。
    pub(crate) fn replace_remove_many(
        &mut self,
        replacements: &[(ObjectId, Annotation)],
        removals: &[ObjectId],
    ) {
        let mut changes: Vec<_> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, object)| {
                if removals.contains(&object.id) {
                    return None;
                }
                let (_, after) = replacements.iter().rev().find(|(id, _)| *id == object.id)?;
                if object.annotation == *after {
                    return None;
                }
                Some(Change::Replace {
                    index,
                    before: object.annotation.clone(),
                    after: after.clone(),
                })
            })
            .collect();
        changes.extend(
            self.items
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, object)| removals.contains(&object.id))
                .map(|(index, object)| Change::Remove {
                    index,
                    object: object.clone(),
                }),
        );
        if !changes.is_empty() {
            self.commit(Change::Batch(changes));
        }
    }
    pub fn replace_many_grouped(&mut self, objects: &[(ObjectId, Annotation)], group: Option<u64>) {
        self.replace_in_group(objects, group.map(EditGroup::Wheel));
    }
    pub(crate) fn replace_watermark_grouped(
        &mut self,
        id: ObjectId,
        annotation: Annotation,
        group: Option<crate::watermark_edit::HistoryToken>,
    ) {
        self.replace_in_group(&[(id, annotation)], group.map(EditGroup::Watermark));
    }
    fn replace_in_group(&mut self, objects: &[(ObjectId, Annotation)], group: Option<EditGroup>) {
        let changes: Vec<_> = objects
            .iter()
            .filter_map(|(id, after)| {
                let index = self.items.iter().position(|o| o.id == *id)?;
                let before = self.items[index].annotation.clone();
                (before != *after).then(|| Change::Replace {
                    index,
                    before,
                    after: after.clone(),
                })
            })
            .collect();
        if !changes.is_empty() {
            // 同一轮滚动保留首次 before，只更新末次 after；其他命令会切断合并。
            if group.is_some() && self.merge_group == group {
                self.edit_revision += 1;
                self.apply(&Change::Batch(changes.clone()), true);
                if let Some(Change::Batch(previous)) = self.undo.last_mut() {
                    for change in changes {
                        if let Change::Replace { index, after, .. } = &change {
                            if let Some(Change::Replace {
                                after: old_after, ..
                            }) = previous.iter_mut().find(
                                |c| matches!(c, Change::Replace { index: old, .. } if old == index),
                            ) {
                                *old_after = after.clone();
                                continue;
                            }
                        }
                        previous.push(change);
                    }
                }
                self.redo.clear();
                if self.undo.last().is_some_and(|c| matches!(c, Change::Batch(items) if items.iter().all(|c| matches!(c, Change::Replace { before, after, .. } if before == after)))) {
                    self.undo.pop();
                    self.merge_group = None;
                }
            } else {
                self.commit(Change::Batch(changes));
                self.merge_group = group;
            }
        }
    }
    pub fn expand_groups(&self, ids: &[ObjectId]) -> Vec<ObjectId> {
        self.expand_groups_in_scope(ids, &[])
    }
    pub(crate) fn expand_groups_in_scope(&self, ids: &[ObjectId], scope: &[u64]) -> Vec<ObjectId> {
        let units: std::collections::BTreeSet<_> = self
            .items
            .iter()
            .filter(|object| ids.contains(&object.id))
            .filter_map(|object| object_groups::unit(object, scope))
            .collect();
        self.items
            .iter()
            .filter(|object| {
                object_groups::unit(object, scope).is_some_and(|unit| units.contains(&unit))
            })
            .map(|object| object.id)
            .collect()
    }
    pub(crate) fn valid_group_scope(&self, scope: &[u64]) -> Vec<u64> {
        object_groups::valid_scope(&self.items, scope)
    }
    pub(crate) fn in_group_scope(&self, id: ObjectId, scope: &[u64]) -> bool {
        self.items
            .iter()
            .find(|object| object.id == id)
            .is_some_and(|object| object_groups::scope_contains(object, scope))
    }
    pub(crate) fn child_group(&self, id: ObjectId, scope: &[u64]) -> Option<u64> {
        self.items
            .iter()
            .find(|object| object.id == id)
            .and_then(|object| match object_groups::unit(object, scope)? {
                object_groups::GroupUnit::Group(group) => Some(group),
                object_groups::GroupUnit::Object(_) => None,
            })
    }
    pub fn grouped(&self, id: ObjectId) -> bool {
        self.child_group(id, &[]).is_some()
    }
    pub fn set_group(&mut self, ids: &[ObjectId], grouped: bool) -> bool {
        self.set_group_in_scope(ids, &[], grouped)
    }
    pub(crate) fn can_group_in_scope(&self, ids: &[ObjectId], scope: &[u64]) -> bool {
        if self.valid_group_scope(scope) != scope || self.next_group >= MAX_HISTORY_ID {
            return false;
        }
        let ids = self.expand_groups_in_scope(ids, scope);
        if ids.len() > MAX_HISTORY_OBJECTS {
            return false;
        }
        let objects: Vec<_> = self
            .items
            .iter()
            .filter(|object| ids.contains(&object.id))
            .collect();
        if objects
            .iter()
            .any(|object| object.group_path().len() >= object_groups::MAX_GROUP_DEPTH)
        {
            return false;
        }
        let units: std::collections::BTreeSet<_> = objects
            .iter()
            .filter_map(|object| object_groups::unit(object, scope))
            .collect();
        units.len() >= 2
    }
    pub(crate) fn set_group_in_scope(
        &mut self,
        ids: &[ObjectId],
        scope: &[u64],
        grouped: bool,
    ) -> bool {
        if self.valid_group_scope(scope) != scope {
            return false;
        }
        let ids = self.expand_groups_in_scope(ids, scope);
        let units: std::collections::BTreeSet<_> = self
            .items
            .iter()
            .filter(|object| ids.contains(&object.id))
            .filter_map(|object| object_groups::unit(object, scope))
            .collect();
        if ids.is_empty() || ids.len() > MAX_HISTORY_OBJECTS || (grouped && units.len() < 2) {
            return false;
        }
        let next_group = if grouped {
            let Some(next) = self
                .next_group
                .checked_add(1)
                .filter(|next| *next <= MAX_HISTORY_ID)
            else {
                return false;
            };
            next
        } else {
            self.next_group
        };
        let before: Vec<_> = self
            .items
            .iter()
            .filter(|object| ids.contains(&object.id))
            .map(|object| (object.id, object.membership()))
            .collect();
        let mut after = Vec::with_capacity(before.len());
        for (id, membership) in &before {
            let mut path = membership.path();
            if grouped {
                if path.len() >= object_groups::MAX_GROUP_DEPTH {
                    return false;
                }
                path.insert(scope.len(), next_group);
            } else if path.len() > scope.len() {
                path.remove(scope.len());
            }
            after.push((*id, GroupMembership::from_path(&path)));
        }
        if before == after {
            return false;
        }
        let mut candidate = self.items.clone();
        for object in &mut candidate {
            if let Some((_, membership)) = after.iter().find(|(id, _)| *id == object.id) {
                object.set_membership(membership);
            }
        }
        if object_groups::validate_forest(&candidate, next_group).is_err() {
            return false;
        }
        let flat = before
            .iter()
            .chain(&after)
            .all(|(_, membership)| membership.nested_groups.is_empty());
        let change = if flat {
            Change::Groups {
                before: before
                    .into_iter()
                    .map(|(id, membership)| (id, membership.group))
                    .collect(),
                after: after
                    .into_iter()
                    .map(|(id, membership)| (id, membership.group))
                    .collect(),
            }
        } else {
            Change::NestedGroups { before, after }
        };
        self.next_group = next_group;
        self.commit(change);
        true
    }
    /// scope 必须仍是当前文档真实存在的完整祖先前缀；调用者在提交前核资格。
    pub(crate) fn insert_in_group_scope(
        &mut self,
        annotation: Annotation,
        scope: &[u64],
    ) -> ObjectId {
        let scope = self.valid_group_scope(scope);
        self.next_id += 1;
        let id = self.next_id;
        let membership = GroupMembership::from_path(&scope);
        self.commit(Change::Insert {
            index: self.items.len(),
            object: Object {
                id,
                annotation,
                group: membership.group,
                nested_groups: membership.nested_groups,
            },
        });
        id
    }
    pub fn duplicate(&mut self, ids: &[ObjectId], canvas: crate::geometry::Rect) -> Vec<ObjectId> {
        self.duplicate_in_group_scope(ids, canvas, &[])
    }
    pub(crate) fn duplicate_in_group_scope(
        &mut self,
        ids: &[ObjectId],
        _canvas: crate::geometry::Rect,
        scope: &[u64],
    ) -> Vec<ObjectId> {
        if self.valid_group_scope(scope) != scope {
            return Vec::new();
        }
        let originals: Vec<_> = self
            .items
            .iter()
            .filter(|o| ids.contains(&o.id) && object_groups::scope_contains(o, scope))
            .cloned()
            .collect();
        // 复制固定错开 12px，即使副本因此伸出画布。渲染只画与画布相交的部分。
        let dx = 12.;
        let dy = 12.;
        let remapped_groups: std::collections::BTreeSet<_> = originals
            .iter()
            .flat_map(|object| object.group_path().into_iter().skip(scope.len()))
            .collect();
        if self
            .next_id
            .checked_add(originals.len() as u64)
            .is_none_or(|next| next > MAX_HISTORY_ID)
            || self
                .next_group
                .checked_add(remapped_groups.len() as u64)
                .is_none_or(|next| next > MAX_HISTORY_ID)
        {
            return Vec::new();
        }
        let mut changes = Vec::new();
        let mut copied = Vec::new();
        let mut groups = std::collections::HashMap::new();
        for mut o in originals {
            // 复制保留离散几何，但成为普通独立效果，不继承智能工具管理权。
            o.annotation.clear_effect_regions_management();
            o.annotation.translate(dx, dy);
            let mut path = o.group_path();
            for group in path.iter_mut().skip(scope.len()) {
                *group = *groups.entry(*group).or_insert_with(|| {
                    self.next_group += 1;
                    self.next_group
                });
            }
            o.set_membership(&GroupMembership::from_path(&path));
            self.next_id += 1;
            o.id = self.next_id;
            copied.push(o.id);
            changes.push(Change::Insert {
                index: self.items.len() + changes.len(),
                object: o,
            });
        }
        if !changes.is_empty() {
            self.commit(Change::Batch(changes));
        }
        copied
    }
    pub fn reorder(&mut self, ids: &[ObjectId], front: bool) {
        let before: Vec<_> = self.items.iter().map(|o| o.id).collect();
        let mut after = before.clone();
        after.sort_by_key(|id| {
            if front {
                ids.contains(id)
            } else {
                !ids.contains(id)
            }
        });
        if before != after {
            self.commit(Change::Order { before, after });
        }
    }
    fn commit(&mut self, change: Change) {
        self.edit_revision += 1;
        self.merge_group = None;
        self.apply(&change, true);
        self.undo.push(change);
        self.redo.clear();
        // 编辑历史有上限，防止长时间标注把全部中间状态一直留在内存。
        if self.undo.len() > 128 {
            self.undo.remove(0);
        }
    }
    fn apply(&mut self, change: &Change, forward: bool) {
        match change {
            Change::LongClip { before, after } => {
                self.long_clip = if forward {
                    after.clone()
                } else {
                    before.clone()
                }
            }
            Change::CropMask { before, after } => {
                self.crop_mask = if forward {
                    after.clone()
                } else {
                    before.clone()
                }
            }
            Change::Batch(changes) => {
                if forward {
                    for c in changes {
                        self.apply(c, true);
                    }
                } else {
                    for c in changes.iter().rev() {
                        self.apply(c, false);
                    }
                }
            }
            Change::Groups { before, after } => {
                for (id, group) in if forward { after } else { before } {
                    if let Some(o) = self.items.iter_mut().find(|o| o.id == *id) {
                        o.group = *group;
                    }
                }
            }
            Change::NestedGroups { before, after } => {
                for (id, membership) in if forward { after } else { before } {
                    if let Some(object) = self.items.iter_mut().find(|object| object.id == *id) {
                        object.set_membership(membership);
                    }
                }
            }
            Change::Order { before, after } => {
                let order = if forward { after } else { before };
                self.items.sort_by_key(|o| {
                    order
                        .iter()
                        .position(|id| *id == o.id)
                        .unwrap_or(usize::MAX)
                });
            }
            Change::Insert { index, object } => {
                if forward {
                    self.items.insert(*index, object.clone());
                } else {
                    self.items.remove(*index);
                }
            }
            Change::Remove { index, object } => {
                if forward {
                    self.items.remove(*index);
                } else {
                    self.items.insert(*index, object.clone());
                }
            }
            Change::Replace {
                index,
                before,
                after,
            } => {
                self.items[*index].annotation = if forward {
                    after.clone()
                } else {
                    before.clone()
                }
            }
            Change::Clear(objects) => {
                if forward {
                    self.items.clear();
                } else {
                    self.items = objects.clone();
                }
            }
        }
        self.revision += 1;
    }
    pub fn undo(&mut self) {
        self.merge_group = None;
        if let Some(change) = self.undo.pop() {
            self.apply(&change, false);
            self.redo.push(change);
        }
    }
    /// 外部历史（如 OCR 排除）绑定的命令仍在栈顶时才能与它一起撤销。
    pub(crate) fn last_undo(&self) -> Option<&Change> {
        self.undo.last()
    }
    pub(crate) fn undo_contains(&self, change: &Change) -> bool {
        self.undo.iter().any(|item| item == change)
    }
    /// 只撤销栈顶的指定命令；另一半历史没有重做，所以同时清空重做栈。
    pub(crate) fn undo_exact(&mut self, change: &Change) -> bool {
        if self.undo.last() != Some(change) {
            return false;
        }
        self.merge_group = None;
        let change = self.undo.pop().expect("已核验栈顶");
        self.apply(&change, false);
        self.redo.clear();
        true
    }
    pub fn redo(&mut self) {
        self.merge_group = None;
        if let Some(change) = self.redo.pop() {
            self.apply(&change, true);
            self.undo.push(change);
        }
    }
    /// 显示层面的整体换色（如文字贴图跟随主题）：当前对象与撤销/重做记录一并改写，
    /// 不提交命令、不计入 edit_revision；返回是否有对象变化。
    pub(crate) fn remap_silently(&mut self, f: impl Fn(&mut Annotation) -> bool) -> bool {
        fn walk(change: &mut Change, f: &impl Fn(&mut Annotation) -> bool) {
            match change {
                Change::Insert { object, .. } | Change::Remove { object, .. } => {
                    f(&mut object.annotation);
                }
                Change::Replace { before, after, .. } => {
                    f(before);
                    f(after);
                }
                Change::Clear(objects) => {
                    for object in objects {
                        f(&mut object.annotation);
                    }
                }
                Change::Batch(changes) => {
                    for change in changes {
                        walk(change, f);
                    }
                }
                Change::LongClip { .. }
                | Change::CropMask { .. }
                | Change::Groups { .. }
                | Change::NestedGroups { .. }
                | Change::Order { .. } => {}
            }
        }
        let mut changed = false;
        for object in &mut self.items {
            changed |= f(&mut object.annotation);
        }
        for change in self.undo.iter_mut().chain(self.redo.iter_mut()) {
            walk(change, &f);
        }
        if changed {
            self.revision += 1;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotation::{Point, Style, Tool};
    fn rectangle() -> Annotation {
        let mut a = Annotation::new(Tool::Rectangle, Point { x: 10., y: 10. }, Style::default());
        a.update(Point { x: 40., y: 30. });
        a
    }
    #[test]
    fn ocr_mark_batch_preserves_layers_and_undoes_insert_replace_remove_together() {
        let mut d = Document::default();
        let original = rectangle();
        let ids = d.insert_many(vec![original.clone(); 3]);
        let mut changed = original.clone();
        changed.style.color = 0x123456;
        let mut added = original.clone();
        added.style.color = 0xabcdef;
        let fresh = d.insert_replace_remove_many(
            vec![added.clone()],
            &[(ids[1], changed.clone())],
            &[ids[0], ids[0], u64::MAX],
        );
        assert_eq!(
            d.items().iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![ids[1], ids[2], fresh[0]]
        );
        assert_eq!(d.get(ids[1]), Some(&changed));
        assert_eq!(d.get(fresh[0]), Some(&added));
        d.undo();
        assert_eq!(d.items().iter().map(|o| o.id).collect::<Vec<_>>(), ids);
        assert!(d.items().iter().all(|o| o.annotation == original));
        d.redo();
        assert_eq!(d.get(ids[1]), Some(&changed));
        assert_eq!(d.get(fresh[0]), Some(&added));
    }
    #[test]
    fn ocr_mark_empty_batch_keeps_redo_and_document_versions() {
        let mut d = Document::default();
        let id = d.insert(rectangle());
        d.undo();
        let revisions = (d.revision(), d.edit_revision());
        assert!(d
            .insert_replace_remove_many(Vec::new(), &[], &[id])
            .is_empty());
        assert_eq!((d.revision(), d.edit_revision()), revisions);
        assert!(d.can_redo());
    }
    #[test]
    fn pin_edit_revision_tracks_merged_content_without_history_navigation() {
        let mut d = Document::default();
        let id = d.insert(rectangle());
        let inserted = d.edit_revision();
        let mut a = rectangle();
        a.style.width = 8.;
        d.replace_many_grouped(&[(id, a.clone())], Some(8));
        let first = d.edit_revision();
        assert!(first > inserted);
        a.style.width = 9.;
        d.replace_many_grouped(&[(id, a.clone())], Some(8));
        let merged = d.edit_revision();
        assert!(merged > first);
        d.replace_many_grouped(&[(id, a)], Some(8));
        d.undo();
        d.redo();
        assert_eq!(d.edit_revision(), merged);
    }
    #[test]
    fn edit_delete_clear_and_branch_preserve_ids_and_order() {
        let mut d = Document::default();
        let id = d.insert(rectangle());
        let other = d.insert(rectangle());
        let mut moved = d.get(id).unwrap().clone();
        moved.translate(20., 10.);
        d.replace(id, moved.clone());
        d.undo();
        assert_eq!(d.get(id), Some(&rectangle()));
        d.redo();
        assert_eq!(d.get(id), Some(&moved));
        d.remove(id);
        d.undo();
        assert_eq!(d.items()[0].id, id);
        assert_eq!(d.items()[1].id, other);
        d.clear();
        d.undo();
        assert_eq!(d.items().len(), 2);
        d.redo();
        assert!(d.items().is_empty());
        d.undo();
        d.remove(other);
        assert!(!d.can_redo());
        assert_eq!(d.items()[0].id, id);
    }
    #[test]
    fn no_op_edit_does_not_clear_redo() {
        let mut d = Document::default();
        let id = d.insert(rectangle());
        d.insert(rectangle());
        d.undo();
        d.replace(id, rectangle());
        assert!(d.can_redo());
    }
    #[test]
    fn wheel_group_returns_to_original_without_spurious_undo() {
        let mut d = Document::default();
        let original = rectangle();
        let id = d.insert(original.clone());
        let mut changed = original.clone();
        changed.style.width += 1.;
        d.replace_many_grouped(&[(id, changed)], Some(1));
        d.replace_many_grouped(&[(id, original.clone())], Some(1));
        assert_eq!(d.get(id), Some(&original));
        d.undo();
        assert!(d.items().is_empty());
    }
}

#[cfg(test)]
mod smart_effect_document_tests {
    use super::*;
    use crate::annotation::{Bounds, Point, Style, Tool};

    fn smart(x: f32) -> Annotation {
        let mut a = Annotation::new(Tool::Mosaic, Point { x, y: 0. }, Style::default());
        a.set_effect_regions(
            vec![
                Bounds {
                    x,
                    y: 0.,
                    width: 10.,
                    height: 10.,
                },
                Bounds {
                    x: x + 20.,
                    y: 20.,
                    width: 10.,
                    height: 10.,
                },
            ],
            true,
        )
        .unwrap();
        a
    }
    #[test]
    fn smart_payload_clear_undo_new_ids_and_manual_replace_undo_follow_real_document() {
        let mut d = Document::default();
        let original = smart(0.);
        let id = d.insert(original.clone());
        d.clear();
        let other = d.insert(smart(40.));
        assert_ne!(id, other);
        d.undo();
        assert!(d.items().is_empty());
        d.undo();
        assert_eq!(d.items()[0].id, id);
        assert_eq!(d.get(id), Some(&original));
        assert!(d.get(id).unwrap().effect_regions_managed());
        d.redo();
        assert!(d.items().is_empty());
        d.redo();
        assert_eq!(d.items()[0].id, other);
        d.undo();
        d.undo();

        let mut edited = original.clone();
        edited.style.block_size = 32;
        edited.clear_effect_regions_management();
        d.replace(id, edited.clone());
        assert!(!d.get(id).unwrap().effect_regions_managed());
        d.undo();
        assert_eq!(d.get(id), Some(&original));
        d.redo();
        assert_eq!(d.get(id), Some(&edited));
    }
    #[test]
    fn smart_copy_detaches_even_at_canvas_edge_and_undo_keeps_parent_geometry() {
        let mut d = Document::default();
        let original = smart(0.);
        let id = d.insert(original.clone());
        let copy = d.duplicate(&[id], crate::geometry::Rect::full(30, 30))[0];
        assert!(!d.get(copy).unwrap().effect_regions_managed());
        let mut stepped = original.effect_regions_world();
        for region in &mut stepped {
            region.x += 12.;
            region.y += 12.;
        }
        assert_eq!(d.get(copy).unwrap().effect_regions_world(), stepped);
        assert!(d.get(copy).unwrap().hit(Point { x: 15., y: 15. }, 0., true));
        assert!(!d.get(id).unwrap().hit(Point { x: 15., y: 15. }, 0., true));
        assert!(d.get(id).unwrap().effect_regions_managed());
        d.undo();
        assert_eq!(d.items().len(), 1);
        assert_eq!(d.get(id), Some(&original));
        d.redo();
        assert_eq!(d.items()[1].id, copy);
        assert!(!d.get(copy).unwrap().effect_regions_managed());
    }
    #[test]
    fn smart_batch_replaces_and_consolidates_once_with_exact_undo_order_and_noop_redo() {
        let mut d = Document::default();
        let first = d.insert(smart(0.));
        let mut ordinary = smart(40.);
        ordinary.clear_effect_regions_management();
        let middle = d.insert(ordinary.clone());
        let target = d.insert(smart(80.));
        let tail = d.insert(smart(120.));
        let before = d.items().to_vec();
        let merged = smart(160.);
        let edits = d.edit_revision();
        d.replace_remove_many(&[(target, merged.clone())], &[first, tail]);
        assert_eq!(d.edit_revision(), edits + 1);
        assert_eq!(
            d.items().iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![middle, target]
        );
        assert_eq!(d.get(middle), Some(&ordinary));
        assert_eq!(d.get(target), Some(&merged));
        d.undo();
        assert_eq!(d.items(), before.as_slice());
        let revision = d.revision();
        let old_target = d.get(target).unwrap().clone();
        d.replace_remove_many(&[(target, old_target)], &[u64::MAX]);
        assert_eq!(d.revision(), revision);
        assert!(d.can_redo());
        d.redo();
        assert_eq!(
            d.items().iter().map(|o| o.id).collect::<Vec<_>>(),
            vec![middle, target]
        );
        assert_eq!(d.get(target), Some(&merged));
    }

    #[test]
    fn silent_remap_rewrites_items_and_history_without_new_edit() {
        let mut d = Document::default();
        let id = d.insert(smart(0.));
        let moved = smart(5.);
        d.replace(id, moved);
        let removed = d.insert(smart(9.));
        d.remove(removed);
        let edit = d.edit_revision();
        let revision = d.revision();
        let recolor = |a: &mut Annotation| {
            let changed = a.style.color != 0x123456;
            a.style.color = 0x123456;
            changed
        };
        assert!(d.remap_silently(recolor));
        assert_eq!(d.edit_revision(), edit);
        assert!(d.revision() > revision);
        assert!(!d.remap_silently(recolor));
        while d.can_undo() {
            d.undo();
            assert!(d
                .items()
                .iter()
                .all(|o| o.annotation.style.color == 0x123456));
        }
        while d.can_redo() {
            d.redo();
            assert!(d
                .items()
                .iter()
                .all(|o| o.annotation.style.color == 0x123456));
        }
    }
}
