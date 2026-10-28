//! 纯色矢量文字画布复用 Raster DTO；原始历史完整核验后才适配字体／主题。
//! 这里只安装 Document 快照，不创建底图、不逐项 insert，也不改原 Raster 入口。
use super::{
    history, WireGroupMembership, WireObject, WireRasterDocument, MAX_NESTED_GROUP_REFERENCES,
    MAX_OBJECTS, MAX_POINTS, MAX_TEXT,
};
use crate::{
    annotation::Annotation,
    document::{BoundedHistorySnapshot, Document, HistoryCommand, Object},
    editor::Editor,
    geometry::Rect,
    text_format::FontFamily,
    Result,
};
use std::collections::HashMap;

const MAX_IMPORT_BYTES: usize = 512 * 1024;
const MAX_REFERENCES: usize = 8192;
const MAX_REGIONS: usize = 65536;
const MAX_NODES: usize = 8192;
const MAX_DEPTH: usize = 8;
const MAX_STEPS: usize = 128;

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
            .ok_or("矢量文档嵌套组引用溢出")?;
        if self.nested_references > MAX_NESTED_GROUP_REFERENCES {
            return Err("矢量文档嵌套组引用超过容量".into());
        }
        Ok(())
    }
    fn object(&mut self, wire: &WireObject) -> Result<()> {
        wire.validate()?;
        self.nested(wire.group, &wire.nested_groups)?;
        if wire.style.font_name.chars().any(char::is_control) {
            return Err("矢量文字文档字体名称含控制字符".into());
        }
        self.references = self
            .references
            .checked_add(1)
            .ok_or("矢量文档引用计数溢出")?;
        self.points = self
            .points
            .checked_add(wire.points.len())
            .ok_or("矢量文档点数溢出")?;
        self.text = self
            .text
            .checked_add(wire.text.len())
            .ok_or("矢量文档正文计数溢出")?;
        for regions in [&wire.fill_regions, &wire.effect_regions]
            .into_iter()
            .flatten()
        {
            self.regions = self
                .regions
                .checked_add(regions.len())
                .ok_or("矢量文档区域计数溢出")?;
        }
        if self.references > MAX_REFERENCES
            || self.points > MAX_POINTS
            || self.text > MAX_TEXT
            || self.regions > MAX_REGIONS
        {
            return Err("矢量文档超过引用、点数、正文或区域容量".into());
        }
        Ok(())
    }
    fn node(&mut self, depth: usize) -> Result<()> {
        self.nodes = self.nodes.checked_add(1).ok_or("矢量文档命令计数溢出")?;
        if depth > MAX_DEPTH || self.nodes > MAX_NODES {
            return Err("矢量文档历史命令过深或过多".into());
        }
        Ok(())
    }
}

/// 符号字体仅供原始等值与连续性验证，绝不调用 name() 或用于绘制。
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
    fn model(&mut self, wire: &WireObject) -> Result<Annotation> {
        let id = self.id(&wire.style.font_name);
        wire.model_with_fonts(FontFamily(id), &mut |name| FontFamily(self.id(name)))
    }
}
fn object_model(
    wire: &history::WireHistoryObject,
    budget: &mut Budget,
    fonts: &mut FontSymbols,
) -> Result<Object> {
    budget.object(&wire.annotation)?;
    Ok(Object {
        id: wire.id,
        group: wire.annotation.group,
        nested_groups: wire.annotation.nested_groups.clone(),
        annotation: fonts.model(&wire.annotation)?,
    })
}
fn command_model(
    wire: &history::WireCommand,
    budget: &mut Budget,
    fonts: &mut FontSymbols,
    depth: usize,
) -> Result<HistoryCommand> {
    use history::WireCommand as W;
    budget.node(depth)?;
    Ok(match wire {
        W::CropMask { .. } => return Err("矢量文字历史不能包含裁剪遮罩".into()),
        W::Insert { index, object } => HistoryCommand::Insert {
            index: *index,
            object: object_model(object, budget, fonts)?,
        },
        W::Remove { index, object } => HistoryCommand::Remove {
            index: *index,
            object: object_model(object, budget, fonts)?,
        },
        W::Replace {
            index,
            before,
            after,
        } => {
            if before.group.is_some()
                || after.group.is_some()
                || !before.nested_groups.is_empty()
                || !after.nested_groups.is_empty()
            {
                return Err("矢量文字替换历史混入组字段".into());
            }
            budget.object(before)?;
            budget.object(after)?;
            let before = fonts.model(before)?;
            let after = fonts.model(after)?;
            if before == after {
                return Err("矢量文字原始历史包含等值替换".into());
            }
            HistoryCommand::Replace {
                index: *index,
                before,
                after,
            }
        }
        W::Clear { objects } => {
            if objects.is_empty() {
                return Err("矢量文字原始历史包含空清空命令".into());
            }
            HistoryCommand::Clear(
                objects
                    .iter()
                    .map(|o| object_model(o, budget, fonts))
                    .collect::<Result<_>>()?,
            )
        }
        W::Batch { commands } => {
            if commands.is_empty() {
                return Err("矢量文字原始历史包含空批次".into());
            }
            HistoryCommand::Batch(
                commands
                    .iter()
                    .map(|c| command_model(c, budget, fonts, depth + 1))
                    .collect::<Result<_>>()?,
            )
        }
        W::Groups { before, after } => {
            if before == after {
                return Err("矢量文字原始历史包含等值分组".into());
            }
            HistoryCommand::Groups {
                before: before.clone(),
                after: after.clone(),
            }
        }
        W::NestedGroups { before, after } => {
            if before == after {
                return Err("矢量文字原始历史包含等值嵌套分组".into());
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
        W::Order { before, after } => {
            if before == after {
                return Err("矢量文字原始历史包含等值层序".into());
            }
            HistoryCommand::Order {
                before: before.clone(),
                after: after.clone(),
            }
        }
    })
}

/// 额外探针只沿原始 Undo／Redo 的执行方向各遍历一次，共至多 MAX_NODES 节点。
/// 非 Batch 仍交给原 Document 单命令重放；不复制 apply，不误拒不同顶层的往返。
fn probe_step(
    document: &mut Document,
    command: &HistoryCommand,
    forward: bool,
    next_id: u64,
    next_group: u64,
    nodes: &mut usize,
    depth: usize,
) -> Result<()> {
    *nodes = nodes.checked_add(1).ok_or("矢量文字批次探针计数溢出")?;
    if *nodes > MAX_NODES || depth > MAX_DEPTH {
        return Err("矢量文字批次探针节点或深度超出上限".into());
    }
    if let HistoryCommand::Batch(children) = command {
        let before_items = document.items().to_vec();
        let before_mask = document.crop_mask.clone();
        if forward {
            for child in children {
                probe_step(document, child, true, next_id, next_group, nodes, depth + 1)?;
            }
        } else {
            for child in children.iter().rev() {
                probe_step(
                    document,
                    child,
                    false,
                    next_id,
                    next_group,
                    nodes,
                    depth + 1,
                )?;
            }
        }
        if document.items() == before_items.as_slice() && document.crop_mask == before_mask {
            return Err("矢量文字原始历史包含净零批次".into());
        }
    } else {
        apply_probe_step(document, command, forward, next_id, next_group)?;
    }
    Ok(())
}

fn apply_probe_step(
    document: &mut Document,
    command: &HistoryCommand,
    forward: bool,
    next_id: u64,
    next_group: u64,
) -> Result<()> {
    let (undo, redo) = if forward {
        (Vec::new(), vec![command.clone()])
    } else {
        (vec![command.clone()], Vec::new())
    };
    let mut probe = Document::from_bounded_history_snapshot(BoundedHistorySnapshot {
        items: document.items().to_vec(),
        crop_mask: document.crop_mask.clone(),
        long_clip: document.long_clip().cloned(),
        undo,
        redo,
        next_id,
        next_group,
    })?;
    if forward {
        probe.redo();
    } else {
        probe.undo();
    }
    *document = probe;
    Ok(())
}

fn snapshot_probe(snapshot: &BoundedHistorySnapshot) -> Result<Document> {
    Document::from_bounded_history_snapshot(BoundedHistorySnapshot {
        items: snapshot.items.clone(),
        crop_mask: snapshot.crop_mask.clone(),
        long_clip: snapshot.long_clip.clone(),
        undo: Vec::new(),
        redo: Vec::new(),
        next_id: snapshot.next_id,
        next_group: snapshot.next_group,
    })
}

fn validate_batch_effects(snapshot: &BoundedHistorySnapshot) -> Result<()> {
    if !snapshot
        .undo
        .iter()
        .chain(&snapshot.redo)
        .any(|c| matches!(c, HistoryCommand::Batch(_)))
    {
        return Ok(());
    }
    let mut nodes = 0;
    let mut undo = snapshot_probe(snapshot)?;
    for command in snapshot.undo.iter().rev() {
        probe_step(
            &mut undo,
            command,
            false,
            snapshot.next_id,
            snapshot.next_group,
            &mut nodes,
            0,
        )?;
    }
    let mut redo = snapshot_probe(snapshot)?;
    for command in snapshot.redo.iter().rev() {
        probe_step(
            &mut redo,
            command,
            true,
            snapshot.next_id,
            snapshot.next_group,
            &mut nodes,
            0,
        )?;
    }
    Ok(())
}

/// 仅供已通过原始 strict 的显示适配结果；不能用于洗掉外来无变化批次。
fn mapped_probe_step(
    document: &mut Document,
    command: &HistoryCommand,
    forward: bool,
    next_id: u64,
    next_group: u64,
    nodes: &mut usize,
    depth: usize,
) -> Result<Option<HistoryCommand>> {
    *nodes = nodes.checked_add(1).ok_or("矢量文字映射探针计数溢出")?;
    if *nodes > MAX_NODES || depth > MAX_DEPTH {
        return Err("矢量文字映射探针节点或深度超出上限".into());
    }
    if let HistoryCommand::Batch(children) = command {
        let before_items = document.items().to_vec();
        let before_mask = document.crop_mask.clone();
        let mut kept = Vec::new();
        if forward {
            for child in children {
                if let Some(child) =
                    mapped_probe_step(document, child, true, next_id, next_group, nodes, depth + 1)?
                {
                    kept.push(child);
                }
            }
        } else {
            for child in children.iter().rev() {
                if let Some(child) = mapped_probe_step(
                    document,
                    child,
                    false,
                    next_id,
                    next_group,
                    nodes,
                    depth + 1,
                )? {
                    kept.push(child);
                }
            }
            kept.reverse();
        }
        if document.items() == before_items.as_slice() && document.crop_mask == before_mask {
            Ok(None)
        } else {
            Ok(Some(HistoryCommand::Batch(kept)))
        }
    } else {
        apply_probe_step(document, command, forward, next_id, next_group)?;
        Ok(Some(command.clone()))
    }
}

fn normalize_mapped_batches(
    mut snapshot: BoundedHistorySnapshot,
) -> Result<BoundedHistorySnapshot> {
    snapshot.ensure_no_long_clip()?;
    Document::from_bounded_history_snapshot(snapshot.clone())?;
    if !snapshot
        .undo
        .iter()
        .chain(&snapshot.redo)
        .any(|c| matches!(c, HistoryCommand::Batch(_)))
    {
        return Ok(snapshot);
    }
    let mut nodes = 0;
    let mut undo = snapshot_probe(&snapshot)?;
    let mut kept_undo = Vec::new();
    for command in snapshot.undo.iter().rev() {
        if let Some(command) = mapped_probe_step(
            &mut undo,
            command,
            false,
            snapshot.next_id,
            snapshot.next_group,
            &mut nodes,
            0,
        )? {
            kept_undo.push(command);
        }
    }
    kept_undo.reverse();
    let mut redo = snapshot_probe(&snapshot)?;
    let mut kept_redo = Vec::new();
    for command in snapshot.redo.iter().rev() {
        if let Some(command) = mapped_probe_step(
            &mut redo,
            command,
            true,
            snapshot.next_id,
            snapshot.next_group,
            &mut nodes,
            0,
        )? {
            kept_redo.push(command);
        }
    }
    kept_redo.reverse();
    snapshot.undo = kept_undo;
    snapshot.redo = kept_redo;
    Document::from_bounded_history_snapshot(snapshot.clone())?;
    Ok(snapshot)
}

fn raw_snapshot(doc: &WireRasterDocument) -> Result<(BoundedHistorySnapshot, FontSymbols)> {
    doc.validate_shape()?;
    if doc.crop != Some([0, 0, doc.width, doc.height]) || doc.mask.is_some() {
        return Err("矢量文字文档需要完整画布且没有裁剪遮罩".into());
    }
    // V2 宿主封套要求完整身份；旧文字 V1 仍由原入口解码。
    let history = doc
        .history
        .as_ref()
        .ok_or("矢量文字文档缺少身份与历史字段")?;
    history.validate_version(&doc.objects)?;
    if history.ids.len() != doc.objects.len()
        || history.undo.len().saturating_add(history.redo.len()) > MAX_STEPS
    {
        return Err("矢量文字历史版本、身份或步骤数量无效".into());
    }
    let mut budget = Budget::default();
    let mut fonts = FontSymbols::default();
    let items = doc
        .objects
        .iter()
        .zip(&history.ids)
        .map(|(wire, &id)| {
            budget.object(wire)?;
            Ok(Object {
                id,
                group: wire.group,
                nested_groups: wire.nested_groups.clone(),
                annotation: fonts.model(wire)?,
            })
        })
        .collect::<Result<_>>()?;
    let undo = history
        .undo
        .iter()
        .map(|c| command_model(c, &mut budget, &mut fonts, 0))
        .collect::<Result<_>>()?;
    let redo = history
        .redo
        .iter()
        .map(|c| command_model(c, &mut budget, &mut fonts, 0))
        .collect::<Result<_>>()?;
    let snapshot = BoundedHistorySnapshot {
        items,
        crop_mask: None,
        long_clip: None,
        undo,
        redo,
        next_id: history.next_id,
        next_group: history.next_group,
    };
    // 原始完整双栈由同一 Document 核心验证，字体回退／主题映射尚未发生。
    Document::from_bounded_history_snapshot(snapshot.clone())?;
    validate_batch_effects(&snapshot)?;
    Ok((snapshot, fonts))
}

fn strict_typed_command(command: &HistoryCommand, nodes: &mut usize, depth: usize) -> Result<()> {
    *nodes = nodes.checked_add(1).ok_or("矢量文字原始命令计数溢出")?;
    if depth > MAX_DEPTH || *nodes > MAX_NODES {
        return Err("矢量文字原始命令过深或过多".into());
    }
    match command {
        HistoryCommand::LongClip { .. } => return Err("矢量文字历史不能包含长图剪辑".into()),
        HistoryCommand::CropMask { .. } => return Err("矢量文字历史不能包含裁剪遮罩".into()),
        HistoryCommand::Replace { before, after, .. } if before == after => {
            return Err("矢量文字原始历史包含等值替换".into());
        }
        HistoryCommand::Clear(objects) if objects.is_empty() => {
            return Err("矢量文字原始历史包含空清空命令".into());
        }
        HistoryCommand::Groups { before, after } if before == after => {
            return Err("矢量文字原始历史包含等值分组".into());
        }
        HistoryCommand::NestedGroups { before, after } if before == after => {
            return Err("矢量文字原始历史包含等值嵌套分组".into());
        }
        HistoryCommand::Order { before, after } if before == after => {
            return Err("矢量文字原始历史包含等值层序".into());
        }
        HistoryCommand::Batch(commands) => {
            if commands.is_empty() {
                return Err("矢量文字原始历史包含空批次".into());
            }
            for child in commands {
                strict_typed_command(child, nodes, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn wire_from_snapshot(
    snapshot: &BoundedHistorySnapshot,
    size: (u32, u32),
) -> Result<WireRasterDocument> {
    snapshot.ensure_no_long_clip()?;
    let mut doc = WireRasterDocument {
        version: 1,
        width: size.0,
        height: size.1,
        crop: Some([0, 0, size.0, size.1]),
        mask: None,
        objects: snapshot
            .items
            .iter()
            .map(|o| WireObject::from_annotation(&o.annotation, o.group, &o.nested_groups))
            .collect(),
        history: Some(history::WireHistory::from_snapshot(snapshot)?),
        capture_source: None,
    };
    doc.version = doc.required_version();
    Ok(doc)
}

impl WireRasterDocument {
    /// 仅真实 Editor 使用：整体显示映射已可能留下等值节点及净零批次。
    /// 外来 wire／直接 typed 候选必须另走 raw strict，不能借此入口洗掉坏命令。
    pub(crate) fn trusted_vector_text_editor_snapshot(
        editor: &Editor,
    ) -> Result<BoundedHistorySnapshot> {
        editor.document.ensure_no_long_clip()?;
        normalize_mapped_batches(editor.document.bounded_history_snapshot_mapped(|_| {})?)
    }

    /// 真实活动栈经现有 mapped 接口取得受信有界连续后缀；不改活动文档。
    pub(crate) fn from_vector_text_editor(
        editor: &Editor,
        size: (u32, u32),
        light: bool,
    ) -> Result<Self> {
        if editor.canvas != Rect::full(size.0, size.1)
            || editor.crop != Some(Rect::full(size.0, size.1))
            || editor.document.crop_mask.is_some()
            || editor.text_draft.is_some()
            || editor.lasso_draft.is_some()
            || editor.is_dragging()
            || editor.has_pending_drawing()
            || editor.watermark.has_preview()
        {
            return Err("矢量文字快照需要完整画布和已完成的草稿".into());
        }
        Self::from_vector_text_snapshot(
            Self::trusted_vector_text_editor_snapshot(editor)?,
            size,
            light,
        )
    }

    /// typed 入口也先拒绝所有原始子节点错误，不能借显示映射／容量裁剪掩盖。
    pub(crate) fn from_vector_text_snapshot(
        snapshot: BoundedHistorySnapshot,
        size: (u32, u32),
        light: bool,
    ) -> Result<Self> {
        snapshot.ensure_no_long_clip()?;
        if snapshot.crop_mask.is_some()
            || snapshot.items.len() > MAX_OBJECTS
            || snapshot.undo.len().saturating_add(snapshot.redo.len()) > MAX_STEPS
        {
            return Err("矢量文字原始快照遮罩或数量无效".into());
        }
        let mut nodes = 0;
        for command in snapshot.undo.iter().chain(&snapshot.redo) {
            strict_typed_command(command, &mut nodes, 0)?;
        }
        Document::from_bounded_history_snapshot(snapshot.clone())?;
        let raw = wire_from_snapshot(&snapshot, size)?;
        raw_snapshot(&raw)?;
        let mapped = snapshot.map_annotations(|a| {
            crate::pin_text_source::remap_theme(a, light, true);
        })?;
        let mapped = normalize_mapped_batches(mapped)?;
        let mut doc = wire_from_snapshot(&mapped, size)?;
        // 映射前原始 candidate 已全量 strict；结构容量不裁，仅为 JSON 字节裁远底。
        raw_snapshot(&doc)?;
        while serde_json::to_vec(&doc)?.len() > history::MAX_WIRE_BYTES {
            if !doc.trim_oldest_history() {
                return Err("矢量文字当前文档超过保存容量".into());
            }
        }
        doc.validate_vector_text()?;
        Ok(doc)
    }

    /// 仅核验原始文档与完整双栈，不枚举本机字体，也不映射主题或裁外来历史。
    pub(crate) fn validate_vector_text(&self) -> Result<()> {
        if serde_json::to_vec(self)?.len() > MAX_IMPORT_BYTES {
            return Err("矢量文字恢复文档超过512KiB".into());
        }
        raw_snapshot(self)?;
        Ok(())
    }

    /// 所有解码和映射完成后才返回新 Document；调用者成功后一次安装，失败保持目标。
    pub(crate) fn decode_vector_text(&self, light: bool) -> Result<Document> {
        if serde_json::to_vec(self)?.len() > MAX_IMPORT_BYTES {
            return Err("矢量文字恢复文档超过512KiB".into());
        }
        let (snapshot, fonts) = raw_snapshot(self)?;
        let actual_fonts: Vec<_> = fonts
            .names
            .iter()
            .map(|name| FontFamily::named(name))
            .collect();
        let mapped = snapshot.map_annotations(|a| {
            a.style.text.font = actual_fonts[a.style.text.font.0];
            if let Some(rich) = &mut a.rich {
                std::sync::Arc::make_mut(rich).map_fonts(|font| actual_fonts[font.0]);
            }
            crate::pin_text_source::remap_theme(a, true, light);
        })?;
        let mapped = normalize_mapped_batches(mapped)?;
        Document::from_bounded_history_snapshot(mapped)
    }
}
