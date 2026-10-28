//! 敏感候选的单对象增量提交；来源、正文与预览身份由宿主完整绑定。
use crate::{
    annotation::{Annotation, Bounds, Point, Style, Tool},
    document::Document,
    Result,
};
use std::sync::Arc;

pub(crate) struct SensitiveEffectEdit<S> {
    revision: u64,
    source: S,
    preview_identity: Arc<()>,
    annotation: Annotation,
}

impl<S: Clone + PartialEq> SensitiveEffectEdit<S> {
    /// 空选择不产生计划；有效计划只含一个非智能托管复合效果。
    /// S 必须包括实际安装代次与正文身份/修订，不能只记录图像或正文 bytes。
    pub(crate) fn prepare(
        document: &Document,
        source: S,
        preview_identity: Arc<()>,
        regions: Vec<Bounds>,
        tool: Tool,
        style: Style,
    ) -> Result<Option<Self>> {
        let Some(first) = regions.first().copied() else {
            return Ok(None);
        };
        if !matches!(tool, Tool::Mosaic | Tool::Blur)
            || !style.opacity.is_finite()
            || !(0. ..=1.).contains(&style.opacity)
        {
            return Err("敏感候选需要有效的马赛克或模糊参数".into());
        }
        let mut annotation = Annotation::new(
            tool,
            Point {
                x: first.x,
                y: first.y,
            },
            style,
        );
        annotation.set_effect_regions(regions, false)?;
        if !annotation.valid() {
            return Err("敏感候选复合区域无效".into());
        }
        Ok(Some(Self {
            revision: document.revision(),
            source,
            preview_identity,
            annotation,
        }))
    }

    /// 宿主在写历史前用返回的整组对象准备 Scene；过期计划不提供旧文档快照。
    pub(crate) fn annotations(&self, document: &Document) -> Option<Vec<Annotation>> {
        if document.revision() != self.revision {
            return None;
        }
        let mut annotations: Vec<_> = document
            .items()
            .iter()
            .map(|object| object.annotation.clone())
            .collect();
        annotations.push(self.annotation.clone());
        Some(annotations)
    }

    /// 只有显式确认、Scene 准备成功且终核来源仍相同时才写一条 Insert。
    /// authorized 由宿主的真实确认、输入状态与 Scene 错误检查共同产生。
    pub(crate) fn commit(
        self,
        document: &mut Document,
        current_source: &S,
        current_preview_identity: &Arc<()>,
        authorized: bool,
    ) -> bool {
        if !authorized
            || document.revision() != self.revision
            || &self.source != current_source
            || !Arc::ptr_eq(&self.preview_identity, current_preview_identity)
        {
            return false;
        }
        document.insert(self.annotation);
        true
    }
}

#[cfg(test)]
#[path = "ocr_sensitive_effect_tests.rs"]
mod tests;
