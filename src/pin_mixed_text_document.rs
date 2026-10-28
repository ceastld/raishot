//! 文字来源 version 2 的矢量组合外壳；复用 Raster DTO，不生成图片或恢复插入命令。
use super::WireTextDocument;
use crate::{
    annotation::{Annotation, LocalFillGeometry, Tool},
    document::{BoundedHistorySnapshot, Document, HistoryCommand},
    editor::Editor,
    pin_raster_document::WireRasterDocument,
    Result,
};

const MAX_SAVE_BYTES: usize = 480 * 1024;
const MAX_IMPORT_BYTES: usize = 512 * 1024;

fn mixed_annotation(annotation: &Annotation) -> bool {
    annotation.tool != Tool::Text
        || annotation.points.len() != 2
        || !matches!(annotation.fill_geometry, LocalFillGeometry::Stroke)
        || annotation.effect_regions.is_some()
}

fn mixed_command(command: &HistoryCommand) -> bool {
    match command {
        HistoryCommand::Insert { object, .. } | HistoryCommand::Remove { object, .. } => {
            mixed_annotation(&object.annotation)
        }
        HistoryCommand::Replace { before, after, .. } => {
            mixed_annotation(before) || mixed_annotation(after)
        }
        HistoryCommand::Clear(objects) => objects
            .iter()
            .any(|object| mixed_annotation(&object.annotation)),
        HistoryCommand::Batch(commands) => commands.iter().any(mixed_command),
        HistoryCommand::CropMask { .. } | HistoryCommand::LongClip { .. } => true,
        HistoryCommand::Groups { .. }
        | HistoryCommand::NestedGroups { .. }
        | HistoryCommand::Order { .. } => false,
    }
}

pub(super) fn contains_mixed(snapshot: &BoundedHistorySnapshot) -> bool {
    snapshot
        .items
        .iter()
        .any(|object| mixed_annotation(&object.annotation))
        || snapshot
            .undo
            .iter()
            .chain(&snapshot.redo)
            .any(mixed_command)
}

pub(super) fn from_editor(
    editor: &Editor,
    size: (u32, u32),
    light: bool,
) -> Result<WireTextDocument> {
    let raster = WireRasterDocument::from_vector_text_editor(editor, size, light)?;
    let mut wire = WireTextDocument {
        version: 2,
        width: size.0,
        height: size.1,
        objects: Vec::new(),
        history: None,
        mixed_document: Some(raster),
    };
    // 内层已完整验证原始语义；外壳字节预算只能裁候选最远历史，不触碰活动文档。
    wire.validate()?;
    while serde_json::to_vec(&wire)?.len() > MAX_SAVE_BYTES {
        if !wire.trim_oldest_history() {
            return Err("混合文字贴图当前文档超过480KiB保存容量".into());
        }
    }
    wire.validate()?;
    Ok(wire)
}

pub(super) fn validate_shape(wire: &WireTextDocument) -> Result<()> {
    let raster = wire
        .mixed_document
        .as_ref()
        .ok_or("混合文字贴图缺少矢量文档")?;
    if wire.version != 2
        || !wire.objects.is_empty()
        || wire.history.is_some()
        || raster.size() != (wire.width, wire.height)
        || serde_json::to_vec(wire)?.len() > MAX_IMPORT_BYTES
    {
        return Err("混合文字贴图版本、尺寸、重复载荷或字节容量无效".into());
    }
    // 尺寸上限、完整画布、无 mask 以及全部当前／历史载荷由严格 bridge 验证。
    Ok(())
}

pub(super) fn decode(wire: &WireTextDocument, light: bool) -> Result<Document> {
    validate_shape(wire)?;
    wire.mixed_document
        .as_ref()
        .ok_or("混合文字贴图缺少矢量文档")?
        .decode_vector_text(light)
}

pub(super) fn validate(wire: &WireTextDocument) -> Result<()> {
    validate_shape(wire)?;
    wire.mixed_document
        .as_ref()
        .ok_or("混合文字贴图缺少矢量文档")?
        .validate_vector_text()
}
