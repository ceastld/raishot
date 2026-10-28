//! 仅编码真实 Document 增量；重放、连续性与分配器校验仍由 Document 负责。
use super::{
    WireGroupMembership, WireObject, WireRasterDocument, MAX_NESTED_GROUP_REFERENCES, MAX_POINTS,
    MAX_TEXT,
};
use crate::{
    annotation::Point,
    crop_mask::Polygon,
    document::{BoundedHistorySnapshot, Document, HistoryCommand, Object},
    Result,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// 给 PinStore 的外观、窗口及 JSON 外壳留出空间；不通过丢当前对象满足预算。
pub(super) const MAX_WIRE_BYTES: usize = 480 * 1024;
const MAX_REFERENCES: usize = 8192;
const MAX_REGIONS: usize = 65536;
const MAX_NODES: usize = 8192;
const MAX_DEPTH: usize = 8;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WireHistory {
    pub(super) version: u32,
    pub(super) ids: Vec<u64>,
    pub(super) next_id: u64,
    pub(super) next_group: u64,
    pub(super) undo: Vec<WireCommand>,
    pub(super) redo: Vec<WireCommand>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct WireHistoryObject {
    pub(super) id: u64,
    pub(super) annotation: WireObject,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(super) enum WireCommand {
    CropMask {
        before: Option<Vec<[f32; 2]>>,
        after: Option<Vec<[f32; 2]>>,
    },
    Insert {
        index: usize,
        object: WireHistoryObject,
    },
    Replace {
        index: usize,
        before: WireObject,
        after: Box<WireObject>,
    },
    Remove {
        index: usize,
        object: WireHistoryObject,
    },
    Clear {
        objects: Vec<WireHistoryObject>,
    },
    Batch {
        commands: Vec<WireCommand>,
    },
    Groups {
        before: Vec<(u64, Option<u64>)>,
        after: Vec<(u64, Option<u64>)>,
    },
    NestedGroups {
        before: Vec<(u64, WireGroupMembership)>,
        after: Vec<(u64, WireGroupMembership)>,
    },
    Order {
        before: Vec<u64>,
        after: Vec<u64>,
    },
}
fn mask_wire(mask: &Option<Polygon>) -> Option<Vec<[f32; 2]>> {
    mask.as_ref()
        .map(|m| m.points.iter().map(|p| [p.x, p.y]).collect())
}
fn mask_model(mask: &Option<Vec<[f32; 2]>>, budget: &mut Budget) -> Result<Option<Polygon>> {
    mask.as_ref()
        .map(|points| {
            budget.points = budget
                .points
                .checked_add(points.len())
                .ok_or("贴图历史点数溢出")?;
            budget.check()?;
            if points.iter().flatten().any(|p| !p.is_finite()) {
                return Err("贴图历史套索坐标无效".into());
            }
            Polygon::new(
                &points
                    .iter()
                    .map(|p| Point { x: p[0], y: p[1] })
                    .collect::<Vec<_>>(),
            )
            .ok_or_else(|| "贴图历史套索轮廓无效".into())
        })
        .transpose()
}
impl WireHistoryObject {
    fn from_object(object: &Object) -> Self {
        Self {
            id: object.id,
            annotation: WireObject::from_annotation(
                &object.annotation,
                object.group,
                &object.nested_groups,
            ),
        }
    }
    fn model(&self, budget: &mut Budget) -> Result<Object> {
        budget.object(&self.annotation)?;
        Ok(Object {
            id: self.id,
            annotation: self.annotation.model()?,
            group: self.annotation.group,
            nested_groups: self.annotation.nested_groups.clone(),
        })
    }
}
impl WireCommand {
    pub(super) fn requires_nested_version(&self) -> bool {
        match self {
            Self::Insert { object, .. } | Self::Remove { object, .. } => {
                !object.annotation.nested_groups.is_empty()
            }
            Self::Replace { before, after, .. } => {
                !before.nested_groups.is_empty() || !after.nested_groups.is_empty()
            }
            Self::Clear { objects } => objects
                .iter()
                .any(|o| !o.annotation.nested_groups.is_empty()),
            Self::Batch { commands } => commands.iter().any(Self::requires_nested_version),
            Self::NestedGroups { .. } => true,
            Self::CropMask { .. } | Self::Groups { .. } | Self::Order { .. } => false,
        }
    }
    fn uses_rich(&self) -> bool {
        match self {
            Self::Insert { object, .. } | Self::Remove { object, .. } => {
                object.annotation.rich.is_some()
            }
            Self::Replace { before, after, .. } => before.rich.is_some() || after.rich.is_some(),
            Self::Clear { objects } => objects.iter().any(|o| o.annotation.rich.is_some()),
            Self::Batch { commands } => commands.iter().any(Self::uses_rich),
            Self::CropMask { .. }
            | Self::Groups { .. }
            | Self::NestedGroups { .. }
            | Self::Order { .. } => false,
        }
    }
    fn from_command(command: &HistoryCommand) -> Result<Self> {
        Ok(match command {
            HistoryCommand::LongClip { .. } => {
                return Err("贴图历史不能包含长图剪辑".into());
            }
            HistoryCommand::CropMask { before, after } => Self::CropMask {
                before: mask_wire(before),
                after: mask_wire(after),
            },
            HistoryCommand::Insert { index, object } => Self::Insert {
                index: *index,
                object: WireHistoryObject::from_object(object),
            },
            HistoryCommand::Replace {
                index,
                before,
                after,
            } => Self::Replace {
                index: *index,
                before: WireObject::from_annotation(before, None, &[]),
                after: Box::new(WireObject::from_annotation(after, None, &[])),
            },
            HistoryCommand::Remove { index, object } => Self::Remove {
                index: *index,
                object: WireHistoryObject::from_object(object),
            },
            HistoryCommand::Clear(objects) => Self::Clear {
                objects: objects.iter().map(WireHistoryObject::from_object).collect(),
            },
            HistoryCommand::Batch(commands) => Self::Batch {
                commands: commands
                    .iter()
                    .map(Self::from_command)
                    .collect::<Result<_>>()?,
            },
            HistoryCommand::Groups { before, after } => Self::Groups {
                before: before.clone(),
                after: after.clone(),
            },
            HistoryCommand::NestedGroups { before, after } => Self::NestedGroups {
                before: before
                    .iter()
                    .map(|(id, membership)| (*id, WireGroupMembership::from(membership)))
                    .collect(),
                after: after
                    .iter()
                    .map(|(id, membership)| (*id, WireGroupMembership::from(membership)))
                    .collect(),
            },
            HistoryCommand::Order { before, after } => Self::Order {
                before: before.clone(),
                after: after.clone(),
            },
        })
    }
    fn model(&self, budget: &mut Budget, depth: usize) -> Result<HistoryCommand> {
        budget.nodes += 1;
        if depth > MAX_DEPTH || budget.nodes > MAX_NODES {
            return Err("贴图历史命令过深或过多".into());
        }
        Ok(match self {
            Self::CropMask { before, after } => HistoryCommand::CropMask {
                before: mask_model(before, budget)?,
                after: mask_model(after, budget)?,
            },
            Self::Insert { index, object } => HistoryCommand::Insert {
                index: *index,
                object: object.model(budget)?,
            },
            Self::Replace {
                index,
                before,
                after,
            } => {
                if before.group.is_some()
                    || after.group.is_some()
                    || !before.nested_groups.is_empty()
                    || !after.nested_groups.is_empty()
                {
                    return Err("贴图替换历史混入组字段".into());
                }
                budget.object(before)?;
                budget.object(after)?;
                HistoryCommand::Replace {
                    index: *index,
                    before: before.model()?,
                    after: after.model()?,
                }
            }
            Self::Remove { index, object } => HistoryCommand::Remove {
                index: *index,
                object: object.model(budget)?,
            },
            Self::Clear { objects } => HistoryCommand::Clear(
                objects
                    .iter()
                    .map(|o| o.model(budget))
                    .collect::<Result<_>>()?,
            ),
            Self::Batch { commands } => HistoryCommand::Batch(
                commands
                    .iter()
                    .map(|c| c.model(budget, depth + 1))
                    .collect::<Result<_>>()?,
            ),
            Self::Groups { before, after } => HistoryCommand::Groups {
                before: before.clone(),
                after: after.clone(),
            },
            Self::NestedGroups { before, after } => {
                let mut model = |entries: &[(u64, WireGroupMembership)]| -> Result<_> {
                    entries
                        .iter()
                        .map(|(id, membership)| {
                            budget.nested(membership.group, &membership.nested_groups)?;
                            Ok((*id, membership.model()?))
                        })
                        .collect::<Result<Vec<_>>>()
                };
                HistoryCommand::NestedGroups {
                    before: model(before)?,
                    after: model(after)?,
                }
            }
            Self::Order { before, after } => HistoryCommand::Order {
                before: before.clone(),
                after: after.clone(),
            },
        })
    }
    fn count(&self, budget: &mut Budget, depth: usize) -> Result<()> {
        budget.nodes += 1;
        if depth > MAX_DEPTH || budget.nodes > MAX_NODES {
            return Err("贴图历史命令过深或过多".into());
        }
        match self {
            Self::CropMask { before, after } => {
                for mask in [before, after].into_iter().flatten() {
                    budget.points += mask.len();
                }
            }
            Self::Insert { object, .. } | Self::Remove { object, .. } => {
                budget.object(&object.annotation)?
            }
            Self::Replace { before, after, .. } => {
                budget.object(before)?;
                budget.object(after)?;
            }
            Self::Clear { objects } => {
                for object in objects {
                    budget.object(&object.annotation)?;
                }
            }
            Self::Batch { commands } => {
                for command in commands {
                    command.count(budget, depth + 1)?;
                }
            }
            Self::NestedGroups { before, after } => {
                for (_, membership) in before.iter().chain(after) {
                    budget.nested(membership.group, &membership.nested_groups)?;
                }
            }
            Self::Groups { .. } | Self::Order { .. } => {}
        }
        budget.check()
    }
}
#[derive(Default)]
struct Budget {
    references: usize,
    points: usize,
    text: usize,
    regions: usize,
    nested_references: usize,
    nodes: usize,
}
impl Budget {
    fn nested(&mut self, group: Option<u64>, inner: &[u64]) -> Result<()> {
        super::validate_group_path(group, inner)?;
        self.nested_references = self
            .nested_references
            .checked_add(inner.len())
            .ok_or("贴图历史嵌套组引用溢出")?;
        self.check()
    }
    fn object(&mut self, object: &WireObject) -> Result<()> {
        self.nested(object.group, &object.nested_groups)?;
        self.references += 1;
        self.points = self
            .points
            .checked_add(object.points.len())
            .ok_or("贴图历史点数溢出")?;
        self.text = self
            .text
            .checked_add(object.text.len())
            .ok_or("贴图历史文字长度溢出")?;
        for regions in [&object.fill_regions, &object.effect_regions]
            .into_iter()
            .flatten()
        {
            self.regions = self
                .regions
                .checked_add(regions.len())
                .ok_or("贴图历史区域数量溢出")?;
        }
        self.check()
    }
    fn check(&self) -> Result<()> {
        if self.references > MAX_REFERENCES
            || self.points > MAX_POINTS
            || self.text > MAX_TEXT
            || self.regions > MAX_REGIONS
            || self.nested_references > MAX_NESTED_GROUP_REFERENCES
        {
            return Err("贴图历史超过对象、点数、文字或效果区域容量".into());
        }
        Ok(())
    }
}
impl WireHistory {
    pub(super) fn from_snapshot(snapshot: &BoundedHistorySnapshot) -> Result<Self> {
        snapshot.ensure_no_long_clip()?;
        let mut wire = Self {
            version: 1,
            ids: snapshot.items.iter().map(|o| o.id).collect(),
            next_id: snapshot.next_id,
            next_group: snapshot.next_group,
            undo: snapshot
                .undo
                .iter()
                .map(WireCommand::from_command)
                .collect::<Result<_>>()?,
            redo: snapshot
                .redo
                .iter()
                .map(WireCommand::from_command)
                .collect::<Result<_>>()?,
        };
        wire.version = if snapshot.items.iter().any(|o| !o.nested_groups.is_empty())
            || wire
                .undo
                .iter()
                .chain(&wire.redo)
                .any(WireCommand::requires_nested_version)
        {
            2
        } else {
            1
        };
        Ok(wire)
    }
    pub(super) fn uses_rich(&self) -> bool {
        self.undo
            .iter()
            .chain(&self.redo)
            .any(WireCommand::uses_rich)
    }
    pub(super) fn refresh_version(&mut self, objects: &[WireObject]) {
        self.version = self.required_version(objects);
    }
    fn required_version(&self, objects: &[WireObject]) -> u32 {
        if objects.iter().any(|o| !o.nested_groups.is_empty())
            || self
                .undo
                .iter()
                .chain(&self.redo)
                .any(WireCommand::requires_nested_version)
        {
            2
        } else {
            1
        }
    }
    pub(super) fn validate_version(&self, objects: &[WireObject]) -> Result<()> {
        if self.version != self.required_version(objects) {
            return Err("贴图历史版本与嵌套载荷不匹配".into());
        }
        Ok(())
    }
    pub(super) fn pop_oldest(&mut self) -> bool {
        // 两栈都只去掉离当前状态最远的底部，最近可执行链保持连续。
        if !self.undo.is_empty() && self.undo.len() >= self.redo.len() {
            self.undo.remove(0);
            true
        } else if !self.redo.is_empty() {
            self.redo.remove(0);
            true
        } else {
            false
        }
    }
}
pub(super) fn fit_budget(doc: &mut WireRasterDocument) -> Result<()> {
    loop {
        let mut budget = Budget::default();
        let payload_fits = (|| -> Result<()> {
            for object in &doc.objects {
                budget.object(object)?;
            }
            // 当前 mask 沿用旧格式的独立 MAX_POINTS 上限；不能因新增历史收紧旧内容合同。
            if let Some(history) = &doc.history {
                for command in history.undo.iter().chain(&history.redo) {
                    command.count(&mut budget, 0)?;
                }
            }
            Ok(())
        })()
        .is_ok();
        if payload_fits && serde_json::to_vec(doc)?.len() <= MAX_WIRE_BYTES {
            return Ok(());
        }
        if !doc.trim_oldest_history() {
            return Err("贴图当前文档超过关闭记录容量".into());
        }
    }
}
pub(super) fn decode(doc: &WireRasterDocument) -> Result<Document> {
    // 旧记录沿用 PinStore 的 512KiB 上限，新导出留出的 32KiB 不成为兼容性门槛。
    if serde_json::to_vec(doc)?.len() > 512 * 1024 {
        return Err("贴图恢复文档超过字节容量".into());
    }
    let mask = mask_model(&doc.mask, &mut Budget::default())?;
    let mut budget = Budget::default();
    let snapshot = if let Some(history) = &doc.history {
        history.validate_version(&doc.objects)?;
        if history.ids.len() != doc.objects.len() || history.undo.len() + history.redo.len() > 128 {
            return Err("贴图历史版本、对象身份或步骤数量无效".into());
        }
        let items = doc
            .objects
            .iter()
            .zip(&history.ids)
            .map(|(wire, &id)| {
                budget.object(wire)?;
                Ok(Object {
                    id,
                    annotation: wire.model()?,
                    group: wire.group,
                    nested_groups: wire.nested_groups.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let undo = history
            .undo
            .iter()
            .map(|c| c.model(&mut budget, 0))
            .collect::<Result<_>>()?;
        let redo = history
            .redo
            .iter()
            .map(|c| c.model(&mut budget, 0))
            .collect::<Result<_>>()?;
        BoundedHistorySnapshot {
            items,
            crop_mask: mask,
            long_clip: None,
            undo,
            redo,
            next_id: history.next_id,
            next_group: history.next_group,
        }
    } else {
        // 旧记录只有当前内容，直接安装空历史；组数值稳定重分配，不提交用户命令。
        let mut groups = HashMap::new();
        let mut next_group = 0;
        let items = doc
            .objects
            .iter()
            .enumerate()
            .map(|(index, wire)| {
                budget.object(wire)?;
                let group = wire.group.map(|old| {
                    *groups.entry(old).or_insert_with(|| {
                        next_group += 1;
                        next_group
                    })
                });
                Ok(Object {
                    id: index as u64 + 1,
                    annotation: wire.model()?,
                    group,
                    nested_groups: Vec::new(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        BoundedHistorySnapshot {
            next_id: items.len() as u64,
            next_group,
            items,
            crop_mask: mask,
            long_clip: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    };
    Document::from_bounded_history_snapshot(snapshot)
}
