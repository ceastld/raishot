//! 图像处理历史只保存严格参数和尺寸；不生成像素、不正规化或裁剪坏记录。
use super::{
    Adjustments, ProcessingHistory, ProcessingHistorySnapshot, MAX_PROCESSING_HISTORY_STEPS,
};
use crate::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WireProcessingHistory {
    version: u32,
    original_width: u32,
    original_height: u32,
    width: u32,
    height: u32,
    current: WireAdjustments,
    #[serde(
        default,
        deserialize_with = "deserialize_history",
        skip_serializing_if = "Option::is_none"
    )]
    history: Option<WireProcessingStacks>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProcessingStacks {
    version: u32,
    undo: Vec<WireAdjustments>,
    redo: Vec<WireAdjustments>,
}

// 仅字段缺失兼容旧记录；显式 null 不能被 Option 默认成空历史。
fn deserialize_history<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<WireProcessingStacks>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    WireProcessingStacks::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireAdjustments {
    turns: u8,
    flip_x: bool,
    flip_y: bool,
    grayscale: bool,
    invert: bool,
    brightness: i16,
}
impl From<Adjustments> for WireAdjustments {
    fn from(value: Adjustments) -> Self {
        Self {
            turns: value.turns,
            flip_x: value.flip_x,
            flip_y: value.flip_y,
            grayscale: value.grayscale,
            invert: value.invert,
            brightness: value.brightness,
        }
    }
}
impl WireAdjustments {
    fn model(self, original: (u32, u32)) -> Result<Adjustments> {
        if self.turns > 3 || !(-100..=100).contains(&self.brightness) {
            return Err("贴图处理历史的旋转或亮度参数无效".into());
        }
        let value = Adjustments {
            turns: self.turns,
            flip_x: self.flip_x,
            flip_y: self.flip_y,
            grayscale: self.grayscale,
            invert: self.invert,
            brightness: self.brightness,
        };
        validate_dimensions(value.dimensions(original.0, original.1))?;
        Ok(value)
    }
}

fn validate_dimensions(size: (u32, u32)) -> Result<()> {
    if size.0 == 0
        || size.1 == 0
        || size.0 > 65535
        || size.1 > 65535
        || u64::from(size.0) * u64::from(size.1) > 32_000_000
    {
        return Err("贴图处理历史的图像尺寸无效".into());
    }
    Ok(())
}

impl WireProcessingHistory {
    pub(crate) fn from_history(
        history: &ProcessingHistory,
        current: Adjustments,
        original: (u32, u32),
        processed: (u32, u32),
    ) -> Result<Self> {
        let snapshot = history.bounded_snapshot(current)?;
        let wire = Self {
            version: 1,
            original_width: original.0,
            original_height: original.1,
            width: processed.0,
            height: processed.1,
            current: snapshot.current.into(),
            history: Some(WireProcessingStacks {
                version: 1,
                undo: snapshot.undo.into_iter().map(Into::into).collect(),
                redo: snapshot.redo.into_iter().map(Into::into).collect(),
            }),
        };
        wire.validate(current, original, processed)?;
        Ok(wire)
    }

    fn snapshot(
        &self,
        expected_current: Adjustments,
        original: (u32, u32),
        processed: (u32, u32),
    ) -> Result<ProcessingHistorySnapshot> {
        validate_dimensions(original)?;
        validate_dimensions(processed)?;
        if self.version != 1
            || (self.original_width, self.original_height) != original
            || (self.width, self.height) != processed
        {
            return Err("贴图处理历史的版本或源图尺寸与宿主不一致".into());
        }
        let current = self.current.model(original)?;
        if current != expected_current || current.dimensions(original.0, original.1) != processed {
            return Err("贴图处理历史的当前参数或处理后尺寸与宿主不一致".into());
        }
        let (undo, redo) = if let Some(history) = &self.history {
            if history.version != 1
                || history.undo.len().saturating_add(history.redo.len())
                    > MAX_PROCESSING_HISTORY_STEPS
            {
                return Err("贴图处理历史的版本或两栈总步数无效".into());
            }
            (
                history
                    .undo
                    .iter()
                    .map(|value| value.model(original))
                    .collect::<Result<Vec<_>>>()?,
                history
                    .redo
                    .iter()
                    .map(|value| value.model(original))
                    .collect::<Result<Vec<_>>>()?,
            )
        } else {
            // 旧参数记录只有 current；直接恢复空栈，不模拟一次 change。
            (Vec::new(), Vec::new())
        };
        Ok(ProcessingHistorySnapshot {
            current,
            undo,
            redo,
        })
    }

    pub(crate) fn validate(
        &self,
        expected_current: Adjustments,
        original: (u32, u32),
        processed: (u32, u32),
    ) -> Result<()> {
        let snapshot = self.snapshot(expected_current, original, processed)?;
        ProcessingHistory::from_bounded_snapshot(snapshot, expected_current)?;
        Ok(())
    }

    /// 只返回声明的原图与处理后尺寸；不校验记录，也不证明原图像素或资产身份。
    pub(crate) fn dimensions(&self) -> ((u32, u32), (u32, u32)) {
        (
            (self.original_width, self.original_height),
            (self.width, self.height),
        )
    }

    /// 宿主必须先以真实 expected_current、original 和 processed 完整 validate。
    /// 语义错误须向上返回；此方法仅裁容量，不是解码或校验入口。
    /// 只移除较长栈底的最远一步（同长先 Undo），不修改当前参数、尺寸或版本。
    pub(crate) fn trim_oldest_history(&mut self) -> bool {
        let Some(history) = &mut self.history else {
            return false;
        };
        if !history.undo.is_empty() && history.undo.len() >= history.redo.len() {
            history.undo.remove(0);
            true
        } else if !history.redo.is_empty() {
            history.redo.remove(0);
            true
        } else {
            false
        }
    }

    /// 所有 wire 门禁先通过；typed restore 完整验证后整体安装，不修改 current。
    pub(crate) fn restore_into(
        &self,
        expected_current: Adjustments,
        original: (u32, u32),
        processed: (u32, u32),
        target: &mut ProcessingHistory,
    ) -> Result<()> {
        let snapshot = self.snapshot(expected_current, original, processed)?;
        target.restore_bounded_snapshot(expected_current, snapshot)
    }
}
