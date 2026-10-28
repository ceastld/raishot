//! 录前有效源范围说明；不改变物理选区，也不描述后续缩放的最终输出尺寸。
use crate::{geometry::ScreenRect, recording_size_layout::RecordingSizeLayout};

fn cropped_layout(region: ScreenRect) -> Option<RecordingSizeLayout> {
    let layout = RecordingSizeLayout::new(region).ok()?;
    (layout.cropped_right() != 0 || layout.cropped_bottom() != 0).then_some(layout)
}

/// 偶数或无效区域沿用原尺寸标签；有效奇数区域额外显示裁边后的源范围。
pub(super) fn size_label(region: ScreenRect) -> String {
    let source = format!("{} × {}", region.width, region.height);
    match cropped_layout(region) {
        Some(layout) => trf!(
            "{source} → 有效源 {} × {}",
            "{source} → source {} × {}",
            layout.encoded_width,
            layout.encoded_height,
        ),
        None => source,
    }
}

/// MP4 与 GIF 的中间录制共用源布局；显式输出缩放另计。
pub(super) fn crop_hint(region: ScreenRect) -> Option<String> {
    let layout = cropped_layout(region)?;
    Some(trf!(
        "右裁 {}、底裁 {} 像素；选区与原点不变。",
        "Crop right {} px / bottom {} px; selection and origin unchanged.",
        layout.cropped_right(),
        layout.cropped_bottom(),
    ))
}
