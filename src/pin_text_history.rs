//! 文字文档只编码真实增量历史；原始命令严格验证后才映射本机字体和主题。
use super::{WireTextDocument, WireTextObject, MAX_OBJECTS, MAX_TEXT_BYTES};
use crate::{
    annotation::Annotation,
    document::{BoundedHistorySnapshot, Document, HistoryCommand, Object},
    pin_raster_document::{validate_group_path, WireGroupMembership, MAX_NESTED_GROUP_REFERENCES},
    text_format::FontFamily,
    Result,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const MAX_WIRE_BYTES: usize = 240 * 1024;
const MAX_CURRENT_WIRE_BYTES: usize = 480 * 1024;
const MAX_IMPORT_BYTES: usize = 512 * 1024;
const MAX_REFERENCES: usize = 4096;
const MAX_TOTAL_TEXT_BYTES: usize = 256 * 1024;
const MAX_STEPS: usize = 128;
const MAX_NODES: usize = 8192;
const MAX_DEPTH: usize = 8;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WireTextHistory {
    version: u32,
    ids: Vec<u64>,
    groups: Vec<Option<u64>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    nested_groups: Vec<Vec<u64>>,
    next_id: u64,
    next_group: u64,
    undo: Vec<WireCommand>,
    redo: Vec<WireCommand>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireHistoryObject {
    id: u64,
    group: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    nested_groups: Vec<u64>,
    annotation: WireTextObject,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum WireCommand {
    Insert {
        index: usize,
        object: WireHistoryObject,
    },
    Remove {
        index: usize,
        object: WireHistoryObject,
    },
    Replace {
        index: usize,
        before: WireTextObject,
        after: Box<WireTextObject>,
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

#[derive(Debug)]
struct TextHistoryBudgetExceeded(&'static str);
impl std::fmt::Display for TextHistoryBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for TextHistoryBudgetExceeded {}

#[derive(Default)]
struct Budget {
    references: usize,
    text_bytes: usize,
    nested_references: usize,
    nodes: usize,
}
impl Budget {
    fn nested(&mut self, group: Option<u64>, inner: &[u64]) -> Result<()> {
        // 结构错误不属于容量错误，不能借裁最远历史洗掉坏路径。
        validate_group_path(group, inner)?;
        self.nested_references = self
            .nested_references
            .checked_add(inner.len())
            .ok_or(TextHistoryBudgetExceeded("文字历史嵌套组引用溢出"))?;
        if self.nested_references > MAX_NESTED_GROUP_REFERENCES {
            return Err(TextHistoryBudgetExceeded("文字历史嵌套组引用超过容量").into());
        }
        Ok(())
    }
    fn object(&mut self, object: &WireTextObject) -> Result<()> {
        // 样式损坏属于语义错误，不能作为容量错误裁掉。
        object.validate()?;
        self.references = self
            .references
            .checked_add(1)
            .ok_or(TextHistoryBudgetExceeded("文字历史引用计数溢出"))?;
        self.text_bytes = self
            .text_bytes
            .checked_add(object.text.len())
            .ok_or(TextHistoryBudgetExceeded("文字历史正文计数溢出"))?;
        if self.references > MAX_REFERENCES || self.text_bytes > MAX_TOTAL_TEXT_BYTES {
            return Err(TextHistoryBudgetExceeded("文字历史引用或正文超过容量").into());
        }
        Ok(())
    }
    fn object_list(&self, length: usize) -> Result<()> {
        if length > MAX_OBJECTS {
            return Err(TextHistoryBudgetExceeded("文字历史对象载荷超过64项").into());
        }
        Ok(())
    }
    fn node(&mut self, depth: usize) -> Result<()> {
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(TextHistoryBudgetExceeded("文字历史节点计数溢出"))?;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err(TextHistoryBudgetExceeded("文字历史节点或嵌套深度超过容量").into());
        }
        Ok(())
    }
}

/// 索引只是严格验证时的名称身份，不调用 name()，也不用于渲染。
#[derive(Default)]
struct FontSymbols {
    names: Vec<String>,
    ids: HashMap<String, usize>,
}
impl FontSymbols {
    fn id(&mut self, name: &str) -> usize {
        *self
            .ids
            .entry(name.to_ascii_lowercase())
            .or_insert_with(|| {
                self.names.push(name.to_owned());
                self.names.len() - 1
            })
    }
    fn model(&mut self, wire: &WireTextObject) -> Result<Annotation> {
        let id = self.id(&wire.style.font_name);
        wire.model_with_fonts(FontFamily(id), &mut |name| FontFamily(self.id(name)))
    }
}

impl WireHistoryObject {
    fn from_object(object: &Object) -> Result<Self> {
        Ok(Self {
            id: object.id,
            group: object.group,
            nested_groups: object.nested_groups.clone(),
            annotation: WireTextObject::from_annotation(&object.annotation)?,
        })
    }
    fn model(&self, budget: &mut Budget, fonts: &mut FontSymbols) -> Result<Object> {
        budget.object(&self.annotation)?;
        budget.nested(self.group, &self.nested_groups)?;
        Ok(Object {
            id: self.id,
            group: self.group,
            nested_groups: self.nested_groups.clone(),
            annotation: fonts.model(&self.annotation)?,
        })
    }
}

impl WireCommand {
    fn requires_nested_version(&self) -> bool {
        match self {
            Self::Insert { object, .. } | Self::Remove { object, .. } => {
                !object.nested_groups.is_empty()
            }
            Self::Clear { objects } => objects.iter().any(|o| !o.nested_groups.is_empty()),
            Self::Batch { commands } => commands.iter().any(Self::requires_nested_version),
            Self::NestedGroups { .. } => true,
            Self::Replace { .. } | Self::Groups { .. } | Self::Order { .. } => false,
        }
    }
    fn from_command(command: &HistoryCommand, nodes: &mut Budget, depth: usize) -> Result<Self> {
        nodes.node(depth)?;
        Ok(match command {
            HistoryCommand::LongClip { .. } => {
                return Err("纯文字历史不能包含长图剪辑".into());
            }
            HistoryCommand::CropMask { .. } => {
                return Err("纯文字历史不能包含裁剪遮罩".into());
            }
            HistoryCommand::Insert { index, object } => Self::Insert {
                index: *index,
                object: WireHistoryObject::from_object(object)?,
            },
            HistoryCommand::Remove { index, object } => Self::Remove {
                index: *index,
                object: WireHistoryObject::from_object(object)?,
            },
            HistoryCommand::Replace {
                index,
                before,
                after,
            } => Self::Replace {
                index: *index,
                before: WireTextObject::from_annotation(before)?,
                after: Box::new(WireTextObject::from_annotation(after)?),
            },
            HistoryCommand::Clear(objects) => Self::Clear {
                objects: objects
                    .iter()
                    .map(WireHistoryObject::from_object)
                    .collect::<Result<_>>()?,
            },
            HistoryCommand::Batch(commands) => Self::Batch {
                commands: commands
                    .iter()
                    .map(|c| Self::from_command(c, nodes, depth + 1))
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
    fn model(
        &self,
        budget: &mut Budget,
        fonts: &mut FontSymbols,
        depth: usize,
    ) -> Result<HistoryCommand> {
        budget.node(depth)?;
        Ok(match self {
            Self::Insert { index, object } => HistoryCommand::Insert {
                index: *index,
                object: object.model(budget, fonts)?,
            },
            Self::Remove { index, object } => HistoryCommand::Remove {
                index: *index,
                object: object.model(budget, fonts)?,
            },
            Self::Replace {
                index,
                before,
                after,
            } => {
                budget.object(before)?;
                budget.object(after)?;
                // 原始语义按符号字体核验；不能让后继显示映射洗掉坏 noop。
                let before = fonts.model(before)?;
                let after = fonts.model(after)?;
                if before == after {
                    return Err("纯文字原始历史包含等值替换".into());
                }
                HistoryCommand::Replace {
                    index: *index,
                    before,
                    after,
                }
            }
            Self::Clear { objects } => {
                if objects.is_empty() {
                    return Err("纯文字原始历史包含空清空命令".into());
                }
                budget.object_list(objects.len())?;
                HistoryCommand::Clear(
                    objects
                        .iter()
                        .map(|o| o.model(budget, fonts))
                        .collect::<Result<_>>()?,
                )
            }
            Self::Batch { commands } => {
                if commands.is_empty() {
                    return Err("纯文字原始历史包含空批次".into());
                }
                HistoryCommand::Batch(
                    commands
                        .iter()
                        .map(|c| c.model(budget, fonts, depth + 1))
                        .collect::<Result<_>>()?,
                )
            }
            Self::Groups { before, after } => {
                budget.object_list(before.len())?;
                budget.object_list(after.len())?;
                if before == after {
                    return Err("纯文字原始历史包含等值分组".into());
                }
                HistoryCommand::Groups {
                    before: before.clone(),
                    after: after.clone(),
                }
            }
            Self::NestedGroups { before, after } => {
                budget.object_list(before.len())?;
                budget.object_list(after.len())?;
                if before == after {
                    return Err("纯文字原始历史包含等值嵌套分组".into());
                }
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
            Self::Order { before, after } => {
                budget.object_list(before.len())?;
                budget.object_list(after.len())?;
                if before == after {
                    return Err("纯文字原始历史包含等值层序".into());
                }
                HistoryCommand::Order {
                    before: before.clone(),
                    after: after.clone(),
                }
            }
        })
    }
}

impl WireCommand {
    fn uses_rich(&self) -> bool {
        match self {
            Self::Insert { object, .. } | Self::Remove { object, .. } => {
                object.annotation.rich.is_some()
            }
            Self::Replace { before, after, .. } => before.rich.is_some() || after.rich.is_some(),
            Self::Clear { objects } => objects.iter().any(|o| o.annotation.rich.is_some()),
            Self::Batch { commands } => commands.iter().any(Self::uses_rich),
            Self::Groups { .. } | Self::NestedGroups { .. } | Self::Order { .. } => false,
        }
    }
}

impl WireTextHistory {
    pub(super) fn uses_rich(&self) -> bool {
        self.undo
            .iter()
            .chain(&self.redo)
            .any(WireCommand::uses_rich)
    }
    pub(super) fn from_snapshot(snapshot: &BoundedHistorySnapshot) -> Result<Self> {
        snapshot.ensure_no_long_clip()?;
        // 已映射的真实快照必须仍然有正确命令链；不以容量裁剪隐藏语义错误。
        Document::from_bounded_history_snapshot(snapshot.clone())?;
        if snapshot.crop_mask.is_some() {
            return Err("纯文字历史不能包含裁剪遮罩".into());
        }
        if snapshot.items.len() > MAX_OBJECTS {
            return Err("纯文字当前对象超过64项".into());
        }
        for object in &snapshot.items {
            WireTextObject::from_annotation(&object.annotation)?;
        }
        let mut nodes = Budget::default();
        let mut wire = Self {
            version: 1,
            ids: snapshot.items.iter().map(|o| o.id).collect(),
            groups: snapshot.items.iter().map(|o| o.group).collect(),
            nested_groups: if snapshot.items.iter().any(|o| !o.nested_groups.is_empty()) {
                snapshot
                    .items
                    .iter()
                    .map(|o| o.nested_groups.clone())
                    .collect()
            } else {
                Vec::new()
            },
            next_id: snapshot.next_id,
            next_group: snapshot.next_group,
            undo: snapshot
                .undo
                .iter()
                .map(|c| WireCommand::from_command(c, &mut nodes, 0))
                .collect::<Result<_>>()?,
            redo: snapshot
                .redo
                .iter()
                .map(|c| WireCommand::from_command(c, &mut nodes, 0))
                .collect::<Result<_>>()?,
        };
        wire.version = wire.required_version();
        Ok(wire)
    }
    fn required_version(&self) -> u32 {
        if self.nested_groups.iter().any(|inner| !inner.is_empty())
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
    fn validate_version(&self) -> Result<()> {
        if self.version != self.required_version() {
            return Err("纯文字历史版本与嵌套载荷不匹配".into());
        }
        Ok(())
    }
    pub(super) fn pop_oldest(&mut self) -> bool {
        // Vec 尾部是下一步执行；只删除较长栈底的最远条目。
        let trimmed = if !self.undo.is_empty() && self.undo.len() >= self.redo.len() {
            self.undo.remove(0);
            true
        } else if !self.redo.is_empty() {
            self.redo.remove(0);
            true
        } else {
            false
        };
        if trimmed {
            self.version = self.required_version();
        }
        trimmed
    }
}

fn current_shape(doc: &WireTextDocument) -> Result<()> {
    if doc.version != doc.pure_version()
        || doc.width == 0
        || doc.height == 0
        || doc.width > 65535
        || doc.height > 65535
        || u64::from(doc.width) * u64::from(doc.height) > 32_000_000
        || (doc.objects.is_empty() && doc.history.is_none())
        || doc.objects.len() > MAX_OBJECTS
    {
        return Err("纯文字来源版本、尺寸或当前对象数量无效".into());
    }
    let mut text_bytes = 0usize;
    for object in &doc.objects {
        object.validate()?;
        text_bytes = text_bytes
            .checked_add(object.text.len())
            .ok_or("纯文字当前正文长度溢出")?;
    }
    if text_bytes > MAX_TEXT_BYTES {
        return Err("纯文字当前正文超过64KiB".into());
    }
    Ok(())
}

fn raw_snapshot(doc: &WireTextDocument) -> Result<(BoundedHistorySnapshot, FontSymbols)> {
    current_shape(doc)?;
    let mut budget = Budget::default();
    let mut fonts = FontSymbols::default();
    let snapshot = if let Some(history) = &doc.history {
        history.validate_version()?;
        if history.ids.len() != doc.objects.len()
            || history.groups.len() != doc.objects.len()
            || !history.nested_groups.is_empty() && history.nested_groups.len() != doc.objects.len()
        {
            return Err("纯文字历史版本或当前身份数组无效".into());
        }
        if history.undo.len().saturating_add(history.redo.len()) > MAX_STEPS {
            return Err(TextHistoryBudgetExceeded("纯文字历史超过128步").into());
        }
        let items = doc
            .objects
            .iter()
            .zip(&history.ids)
            .zip(&history.groups)
            .enumerate()
            .map(|(index, ((wire, &id), &group))| {
                budget.object(wire)?;
                let nested_groups = if history.nested_groups.is_empty() {
                    Vec::new()
                } else {
                    history.nested_groups[index].clone()
                };
                budget.nested(group, &nested_groups)?;
                Ok(Object {
                    id,
                    group,
                    nested_groups,
                    annotation: fonts.model(wire)?,
                })
            })
            .collect::<Result<_>>()?;
        let undo = history
            .undo
            .iter()
            .map(|c| c.model(&mut budget, &mut fonts, 0))
            .collect::<Result<_>>()?;
        let redo = history
            .redo
            .iter()
            .map(|c| c.model(&mut budget, &mut fonts, 0))
            .collect::<Result<_>>()?;
        BoundedHistorySnapshot {
            items,
            crop_mask: None,
            long_clip: None,
            undo,
            redo,
            next_id: history.next_id,
            next_group: history.next_group,
        }
    } else {
        // 旧 current-only 文字记录没有组字段；直接安装空历史，不模拟插入。
        let items = doc
            .objects
            .iter()
            .enumerate()
            .map(|(index, wire)| {
                budget.object(wire)?;
                Ok(Object {
                    id: index as u64 + 1,
                    group: None,
                    nested_groups: Vec::new(),
                    annotation: fonts.model(wire)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        BoundedHistorySnapshot {
            next_id: items.len() as u64,
            next_group: 0,
            items,
            crop_mask: None,
            long_clip: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    };
    Ok((snapshot, fonts))
}

fn replay_capacity(document: &Document) -> Result<()> {
    let mut text_bytes = 0usize;
    for object in document.items() {
        text_bytes = text_bytes
            .checked_add(object.annotation.text.len())
            .ok_or(TextHistoryBudgetExceeded("文字历史状态正文长度溢出"))?;
    }
    if document.items().len() > MAX_OBJECTS || text_bytes > MAX_TEXT_BYTES {
        return Err(TextHistoryBudgetExceeded("文字历史状态超过64对象或64KiB正文").into());
    }
    Ok(())
}

fn validate_raw_snapshot(snapshot: &BoundedHistorySnapshot) -> Result<()> {
    // 先完整 strict，不能让字体缺失、主题映射或容量裁剪掩盖坏 ID、noop 与断链。
    let mut undo = Document::from_bounded_history_snapshot(snapshot.clone())?;
    replay_capacity(&undo)?;
    while undo.can_undo() {
        undo.undo();
        replay_capacity(&undo)?;
    }
    let mut redo = Document::from_bounded_history_snapshot(snapshot.clone())?;
    while redo.can_redo() {
        redo.redo();
        replay_capacity(&redo)?;
    }
    Ok(())
}

pub(super) fn fit_budget(doc: &mut WireTextDocument) -> Result<()> {
    // current 不能被裁掉，也不能因当前损坏而删除全部历史后降级。
    current_shape(doc)?;
    loop {
        let payload_fits =
            match raw_snapshot(doc).and_then(|(snapshot, _)| validate_raw_snapshot(&snapshot)) {
                Ok(()) => true,
                Err(error) if error.is::<TextHistoryBudgetExceeded>() => false,
                Err(error) => return Err(error),
            };
        if payload_fits {
            let wire_bytes = serde_json::to_vec(doc)?.len();
            if wire_bytes <= MAX_WIRE_BYTES {
                return Ok(());
            }
            // JSON 转义可放大合法正文；历史裁空后沿用当前内容容量，保留身份和时钟。
            let no_history_steps = doc
                .history
                .as_ref()
                .is_none_or(|history| history.undo.is_empty() && history.redo.is_empty());
            if no_history_steps && wire_bytes <= MAX_CURRENT_WIRE_BYTES {
                return Ok(());
            }
        }
        if !doc
            .history
            .as_mut()
            .is_some_and(WireTextHistory::pop_oldest)
        {
            return Err("纯文字当前文档超过保存容量".into());
        }
        doc.version = doc.pure_version();
    }
}

pub(super) fn decode(doc: &WireTextDocument, light: bool) -> Result<Document> {
    let (snapshot, fonts) = raw_snapshot(doc)?;
    if serde_json::to_vec(doc)?.len() > MAX_IMPORT_BYTES {
        return Err("纯文字恢复文档超过512KiB".into());
    }
    validate_raw_snapshot(&snapshot)?;
    let actual_fonts: Vec<_> = fonts
        .names
        .iter()
        .map(|name| super::named_font(name))
        .collect();
    let mapped = snapshot.map_annotations(|annotation| {
        annotation.style.text.font = actual_fonts[annotation.style.text.font.0];
        if let Some(rich) = &mut annotation.rich {
            std::sync::Arc::make_mut(rich).map_fonts(|font| actual_fonts[font.0]);
        }
        crate::pin_text_source::remap_theme(annotation, true, light);
    })?;
    Document::from_bounded_history_snapshot(mapped)
}
