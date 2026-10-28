//! 图库工具注册表：工具是对选中内容的动作。按选区里的图片与录屏数量判断
//! 可用或还差什么；图库详情栏、底部操作条和“工具”菜单共用这一份。
use crate::toolbar_icons::Icon;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ToolId {
    View,
    Pin,
    Edit,
    Compare,
    Collage,
    Pdf,
    Batch,
    Split,
    Measure,
    Barcode,
    Gif,
}

/// “工具”菜单顺序，`None` 处画分隔线：多图、单图、录屏。
pub(crate) const MENU: [Option<ToolId>; 10] = [
    Some(ToolId::Compare),
    Some(ToolId::Collage),
    Some(ToolId::Pdf),
    Some(ToolId::Batch),
    None,
    Some(ToolId::Split),
    Some(ToolId::Measure),
    Some(ToolId::Barcode),
    None,
    Some(ToolId::Gif),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Accepts {
    /// 带图片的条目（截图、图片贴图），数量在 min..=max。
    Images { min: usize, max: usize },
    /// 恰好一段录屏，且不混选图片。
    Recording,
    /// 恰好一项图片或录屏。
    One,
}

/// 选区摘要。文字贴图没有像素，既不算图片也不算录屏。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Selection {
    pub images: usize,
    pub recordings: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Availability {
    /// 可用；`partial` 表示混选时只作用于其中的图片。
    Ready {
        partial: bool,
    },
    NeedImages {
        min: usize,
        max: usize,
    },
    NeedRecording,
    NeedOne,
}
impl Availability {
    pub(crate) fn ready(self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

impl ToolId {
    fn accepts(self) -> Accepts {
        const MANY: usize = usize::MAX;
        match self {
            Self::View => Accepts::One,
            Self::Pin | Self::Pdf | Self::Batch => Accepts::Images { min: 1, max: MANY },
            Self::Edit | Self::Split | Self::Measure | Self::Barcode => {
                Accepts::Images { min: 1, max: 1 }
            }
            Self::Compare => Accepts::Images { min: 2, max: 2 },
            Self::Collage => Accepts::Images { min: 2, max: MANY },
            Self::Gif => Accepts::Recording,
        }
    }
    /// 选择模式下这一项能否被勾选。
    pub(crate) fn takes(self, image: bool, recording: bool) -> bool {
        match self.accepts() {
            Accepts::Images { .. } => image,
            Accepts::Recording => recording,
            Accepts::One => image || recording,
        }
    }
    /// 选择模式最多勾几项；满了再点新的一项替换最后一项。
    pub(crate) fn limit(self) -> usize {
        match self.accepts() {
            Accepts::Images { max, .. } => max,
            Accepts::Recording | Accepts::One => 1,
        }
    }
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Pin => "pin",
            Self::Edit => "edit",
            Self::Compare => "compare",
            Self::Collage => "collage",
            Self::Pdf => "pdf",
            Self::Batch => "batch",
            Self::Split => "split",
            Self::Measure => "measure",
            Self::Barcode => "barcode",
            Self::Gif => "gif",
        }
    }
    /// 能直接用外部文件打开的独立工具窗；查看、贴出、编辑与录屏走各自入口。
    pub(crate) fn opens_files(self) -> bool {
        matches!(
            self,
            Self::Compare
                | Self::Collage
                | Self::Pdf
                | Self::Batch
                | Self::Split
                | Self::Measure
                | Self::Barcode
        )
    }
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        [
            Self::View,
            Self::Pin,
            Self::Edit,
            Self::Compare,
            Self::Collage,
            Self::Pdf,
            Self::Batch,
            Self::Split,
            Self::Measure,
            Self::Barcode,
            Self::Gif,
        ]
        .into_iter()
        .find(|tool| tool.id() == id)
    }
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::View => crate::tr!("查看", "View"),
            Self::Pin => crate::tr!("贴出", "Pin"),
            Self::Edit => crate::tr!("编辑", "Edit"),
            Self::Compare => crate::tr!("对照", "Compare"),
            Self::Collage => crate::tr!("拼接", "Collage"),
            Self::Pdf => crate::tr!("转 PDF", "To PDF"),
            Self::Batch => crate::tr!("批量导出", "Batch export"),
            Self::Split => crate::tr!("分割", "Split"),
            Self::Measure => crate::tr!("取色与测量", "Color & measure"),
            Self::Barcode => crate::tr!("识别条码", "Read barcodes"),
            Self::Gif => crate::tr!("转 GIF", "To GIF"),
        }
    }
    /// 选择模式横幅里的一句目标说明。
    pub(crate) fn goal(self) -> &'static str {
        match self {
            Self::Compare => crate::tr!(
                "选择 2 张图片，1 在左、2 在右",
                "Pick 2 images: 1 left, 2 right"
            ),
            Self::Collage => crate::tr!(
                "选择要拼接的图片，至少 2 张，按选择顺序排列",
                "Pick at least 2 images; they join in pick order"
            ),
            Self::Pdf => crate::tr!(
                "选择图片，每张一页，按选择顺序排页",
                "Pick images, one per page in pick order"
            ),
            Self::Batch => crate::tr!("选择要导出的图片", "Pick the images to export"),
            Self::Gif => crate::tr!("选择 1 段录屏", "Pick 1 recording"),
            Self::View => crate::tr!("选择 1 项", "Pick 1 item"),
            Self::Pin => crate::tr!("选择要贴出的图片", "Pick images to pin"),
            Self::Edit | Self::Split | Self::Measure | Self::Barcode => {
                crate::tr!("选择 1 张图片", "Pick 1 image")
            }
        }
    }
    pub(crate) fn icon(self) -> Icon {
        match self {
            Self::View => Icon::Eye,
            Self::Pin => Icon::Pin,
            Self::Edit => Icon::Edit,
            Self::Compare => Icon::Compare,
            Self::Collage => Icon::Collage,
            Self::Pdf => Icon::File,
            Self::Batch => Icon::Images,
            Self::Split => Icon::Grid,
            Self::Measure => Icon::Eyedropper,
            Self::Barcode => Icon::Ocr,
            Self::Gif => Icon::Video,
        }
    }
}

pub(crate) fn availability(tool: ToolId, selection: Selection) -> Availability {
    let Selection { images, recordings } = selection;
    match tool.accepts() {
        Accepts::Images { min, max } if (min..=max).contains(&images) => Availability::Ready {
            partial: recordings > 0,
        },
        Accepts::Images { min, max } => Availability::NeedImages { min, max },
        Accepts::Recording if recordings == 1 && images == 0 => {
            Availability::Ready { partial: false }
        }
        Accepts::Recording => Availability::NeedRecording,
        Accepts::One if images + recordings == 1 => Availability::Ready { partial: false },
        Accepts::One => Availability::NeedOne,
    }
}

/// 详情栏与操作条直接露出的动作，其余在“工具”菜单里。
pub(crate) fn recommended(selection: Selection) -> &'static [ToolId] {
    match (selection.images, selection.recordings) {
        (0, 1) => &[ToolId::View, ToolId::Gif],
        (0, _) => &[],
        (1, 0) => &[ToolId::Pin, ToolId::Edit, ToolId::View],
        (2, _) => &[ToolId::Compare, ToolId::Collage],
        (1, _) => &[ToolId::Pin],
        _ => &[ToolId::Collage, ToolId::Pdf, ToolId::Batch],
    }
}

/// 不可用时菜单右侧的提示。
pub(crate) fn need_text(availability: Availability) -> String {
    match availability {
        Availability::Ready { .. } => String::new(),
        Availability::NeedImages { min, max } if min == max => {
            crate::trf!("选 {} 张图片", "Pick {} images", min)
        }
        Availability::NeedImages { max: 1, .. } => {
            crate::tr!("只选 1 张图片", "Pick 1 image").into()
        }
        Availability::NeedImages { min, .. } => {
            crate::trf!("至少 {} 张图片", "At least {} images", min)
        }
        Availability::NeedRecording => crate::tr!("选 1 段录屏", "Pick 1 recording").into(),
        Availability::NeedOne => crate::tr!("只选 1 项", "Pick 1 item").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(images: usize, recordings: usize) -> Selection {
        Selection { images, recordings }
    }

    #[test]
    fn tool_registry_availability_follows_counts_and_kinds() {
        assert!(availability(ToolId::Compare, sel(2, 0)).ready());
        assert_eq!(
            availability(ToolId::Compare, sel(1, 0)),
            Availability::NeedImages { min: 2, max: 2 }
        );
        assert!(!availability(ToolId::Compare, sel(3, 0)).ready());
        assert!(availability(ToolId::Collage, sel(5, 0)).ready());
        assert!(!availability(ToolId::Split, sel(2, 0)).ready());
        // 混选：图片工具只作用于图片，录屏工具要求单独一段。
        assert_eq!(
            availability(ToolId::Pdf, sel(3, 1)),
            Availability::Ready { partial: true }
        );
        assert!(availability(ToolId::Gif, sel(0, 1)).ready());
        assert_eq!(
            availability(ToolId::Gif, sel(1, 1)),
            Availability::NeedRecording
        );
        assert!(availability(ToolId::View, sel(0, 1)).ready());
        assert_eq!(availability(ToolId::View, sel(2, 0)), Availability::NeedOne);
        assert!(!availability(ToolId::Pin, sel(0, 0)).ready());
    }

    #[test]
    fn tool_registry_recommends_by_selection_shape() {
        assert_eq!(recommended(sel(0, 0)), &[] as &[ToolId]);
        assert_eq!(
            recommended(sel(1, 0)),
            &[ToolId::Pin, ToolId::Edit, ToolId::View]
        );
        assert_eq!(recommended(sel(2, 0)), &[ToolId::Compare, ToolId::Collage]);
        assert_eq!(
            recommended(sel(4, 0)),
            &[ToolId::Collage, ToolId::Pdf, ToolId::Batch]
        );
        assert_eq!(recommended(sel(0, 1)), &[ToolId::View, ToolId::Gif]);
        for &tool in recommended(sel(3, 1)) {
            assert!(availability(tool, sel(3, 1)).ready());
        }
    }

    #[test]
    fn tool_registry_pick_rules() {
        assert!(ToolId::Compare.takes(true, false));
        assert!(!ToolId::Compare.takes(false, true));
        assert!(ToolId::Gif.takes(false, true));
        assert_eq!((ToolId::Split.limit(), ToolId::Gif.limit()), (1, 1));
        assert_eq!(ToolId::Compare.limit(), 2);
        assert_eq!(ToolId::from_id("collage"), Some(ToolId::Collage));
        assert_eq!(ToolId::from_id("tools"), None);
        assert_eq!(ToolId::Collage.limit(), usize::MAX);
        assert!(MENU
            .iter()
            .flatten()
            .all(|tool| !matches!(tool, ToolId::View | ToolId::Pin | ToolId::Edit)));
    }
}
