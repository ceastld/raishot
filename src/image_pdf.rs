//! 图片页 PDF 1.4：只嵌入本次输入的 RGB 像素和绘制指令，不含外部资源或交互动作。
use crate::{
    capture::Image,
    image_tools::{
        check_cancel, require_committed, validate_input, ToolError, ToolLimits, ToolResult,
    },
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

const HEADER: &[u8] = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n";
const CATALOG: &str = "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n";
const IMAGE_END: &[u8] = b"\nendstream\nendobj\n";
const MAX_PAGES: usize = 128;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MIN_PAGE_POINTS: f64 = 3.;
const MAX_PAGE_POINTS: f64 = 14_400.;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PdfPageSize {
    /// 每个源像素占 72/dpi 点；页边距额外加在图像四周。
    ImagePixels {
        dpi: f64,
    },
    A4Portrait,
    A4Landscape,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PdfOptions {
    pub page_size: PdfPageSize,
    pub margin_mm: f64,
}

impl Default for PdfOptions {
    fn default() -> Self {
        Self {
            page_size: PdfPageSize::A4Portrait,
            margin_mm: 10.,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PdfLimits {
    pub max_total_pixels: u64,
    pub max_output_bytes: usize,
    pub max_pages: usize,
}

impl Default for PdfLimits {
    fn default() -> Self {
        Self {
            max_total_pixels: 32_000_000,
            max_output_bytes: MAX_BYTES,
            max_pages: MAX_PAGES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PdfPageLayout {
    pub page_width_pt: f64,
    pub page_height_pt: f64,
    pub image_x_pt: f64,
    pub image_y_pt: f64,
    pub image_width_pt: f64,
    pub image_height_pt: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SavedPdf {
    pub path: PathBuf,
    pub pages: usize,
    pub bytes: usize,
}

/// PDF 默认单位为 1/72 英寸；A4 使用 210×297 mm，居中等比适配且不裁剪。
pub(crate) fn page_layout(
    width: u32,
    height: u32,
    options: PdfOptions,
) -> ToolResult<PdfPageLayout> {
    if width == 0 || height == 0 || !options.margin_mm.is_finite() || options.margin_mm < 0. {
        return Err(invalid(tr!(
            "图片尺寸或 PDF 边距无效",
            "Invalid image dimensions or PDF margin"
        )));
    }
    let margin = options.margin_mm * 72. / 25.4;
    let (page_width, page_height, native_size) = match options.page_size {
        PdfPageSize::ImagePixels { dpi } => {
            if !dpi.is_finite() || !(1. ..=9600.).contains(&dpi) {
                return Err(invalid(tr!(
                    "PDF 图像 DPI 必须为 1–9600",
                    "PDF image DPI must be 1–9600"
                )));
            }
            let image_width = f64::from(width) * 72. / dpi;
            let image_height = f64::from(height) * 72. / dpi;
            (
                image_width + margin * 2.,
                image_height + margin * 2.,
                Some((image_width, image_height)),
            )
        }
        PdfPageSize::A4Portrait => (210. * 72. / 25.4, 297. * 72. / 25.4, None),
        PdfPageSize::A4Landscape => (297. * 72. / 25.4, 210. * 72. / 25.4, None),
    };
    if !page_width.is_finite()
        || !page_height.is_finite()
        || !(MIN_PAGE_POINTS..=MAX_PAGE_POINTS).contains(&page_width)
        || !(MIN_PAGE_POINTS..=MAX_PAGE_POINTS).contains(&page_height)
    {
        return Err(invalid(tr!(
            "PDF 页边长必须为 3–14400 点，请调整 DPI 或边距",
            "PDF page sides must be 3–14400 points; adjust DPI or margins"
        )));
    }
    let available_width = page_width - margin * 2.;
    let available_height = page_height - margin * 2.;
    if available_width <= 0. || available_height <= 0. {
        return Err(invalid(tr!(
            "PDF 边距没有留下可用图像区域",
            "PDF margins leave no space for the image"
        )));
    }
    let (image_width, image_height) = native_size.unwrap_or_else(|| {
        let scale = (available_width / f64::from(width)).min(available_height / f64::from(height));
        (f64::from(width) * scale, f64::from(height) * scale)
    });
    // 六位小数写入 PDF；拒绝会在序列化时舍为零的极小绘制尺寸。
    if !image_width.is_finite()
        || !image_height.is_finite()
        || image_width < 0.000001
        || image_height < 0.000001
    {
        return Err(invalid(tr!(
            "PDF 图像绘制区域过小",
            "PDF image area is too small"
        )));
    }
    Ok(PdfPageLayout {
        page_width_pt: page_width,
        page_height_pt: page_height,
        image_x_pt: (page_width - image_width) / 2.,
        image_y_pt: (page_height - image_height) / 2.,
        image_width_pt: image_width,
        image_height_pt: image_height,
    })
}

fn invalid(message: &str) -> ToolError {
    ToolError::Invalid(message.into())
}

fn size_error() -> ToolError {
    invalid(tr!(
        "PDF 超过输出字节预算或尺寸溢出",
        "PDF exceeds its byte limit or dimensions overflow"
    ))
}

fn add_size(total: usize, added: usize, limit: usize) -> ToolResult<usize> {
    total
        .checked_add(added)
        .filter(|size| *size <= limit)
        .ok_or_else(size_error)
}

struct PageRecord {
    page_object: String,
    content_object: String,
    image_prefix: String,
    rgb_bytes: usize,
}

struct PdfPlan {
    pages: Vec<PageRecord>,
    pages_object: String,
    xref_header: String,
    trailer: String,
    xref_offset: usize,
    output_bytes: usize,
    object_count: usize,
    max_row_bytes: usize,
}

impl PdfPlan {
    fn new(
        images: &[&Image],
        options: PdfOptions,
        limits: PdfLimits,
        cancel: &AtomicBool,
    ) -> ToolResult<Self> {
        if limits.max_pages == 0
            || limits.max_pages > MAX_PAGES
            || limits.max_output_bytes == 0
            || limits.max_output_bytes > MAX_BYTES
            || limits.max_total_pixels == 0
        {
            return Err(invalid(tr!(
                "PDF 预算无效，最多 128 页及 128 MiB",
                "Invalid PDF limits; at most 128 pages and 128 MiB"
            )));
        }
        if images.is_empty() || images.len() > limits.max_pages {
            return Err(invalid(tr!(
                "PDF 图片列表为空或超过页数上限",
                "PDF image list is empty or exceeds the page limit"
            )));
        }
        let image_limits = ToolLimits {
            max_input_pixels: limits.max_total_pixels,
            ..ToolLimits::default()
        };
        let mut pixels = 0_u64;
        let mut max_row_bytes = 0;
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(images.len())
            .map_err(|_| memory_error())?;
        let mut kids = String::new();
        let mut total = add_size(HEADER.len(), CATALOG.len(), limits.max_output_bytes)?;
        for (index, image) in images.iter().enumerate() {
            check_cancel(cancel)?;
            pixels = pixels
                .checked_add(validate_input(image, image_limits)?)
                .filter(|sum| *sum <= limits.max_total_pixels)
                .ok_or_else(|| {
                    invalid(tr!(
                        "PDF 输入合计超过像素预算",
                        "PDF inputs exceed the total pixel limit"
                    ))
                })?;
            let layout = page_layout(image.width, image.height, options)?;
            let page_id = 3 + index * 3;
            let content_id = page_id + 1;
            let image_id = page_id + 2;
            kids.push_str(&format!("{page_id} 0 R "));
            let page_object = format!("{page_id} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {:.6} {:.6}] /Resources << /ProcSet [/PDF /ImageC] /XObject << /Im0 {image_id} 0 R >> >> /Contents {content_id} 0 R >>\nendobj\n", layout.page_width_pt, layout.page_height_pt);
            let content = format!(
                "q\n{:.6} 0 0 {:.6} {:.6} {:.6} cm\n/Im0 Do\nQ\n",
                layout.image_width_pt, layout.image_height_pt, layout.image_x_pt, layout.image_y_pt
            );
            let content_object = format!(
                "{content_id} 0 obj\n<< /Length {} >>\nstream\n{content}endstream\nendobj\n",
                content.len()
            );
            let rgb_bytes = image
                .bgra
                .len()
                .checked_div(4)
                .and_then(|count| count.checked_mul(3))
                .ok_or_else(size_error)?;
            let image_prefix = format!("{image_id} 0 obj\n<< /Type /XObject /Subtype /Image /Width {} /Height {} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Length {rgb_bytes} >>\nstream\n", image.width, image.height);
            for size in [
                page_object.len(),
                content_object.len(),
                image_prefix.len(),
                rgb_bytes,
                IMAGE_END.len(),
            ] {
                total = add_size(total, size, limits.max_output_bytes)?;
            }
            max_row_bytes = max_row_bytes.max(image.width as usize * 4);
            pages.push(PageRecord {
                page_object,
                content_object,
                image_prefix,
                rgb_bytes,
            });
        }
        let pages_object = format!(
            "2 0 obj\n<< /Type /Pages /Kids [{kids}] /Count {} >>\nendobj\n",
            images.len()
        );
        total = add_size(total, pages_object.len(), limits.max_output_bytes)?;
        let xref_offset = total;
        let object_count = 2 + images.len() * 3;
        let size = object_count + 1;
        let xref_header = format!("xref\n0 {size}\n");
        let trailer =
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n");
        total = add_size(total, xref_header.len(), limits.max_output_bytes)?;
        total = add_size(
            total,
            size.checked_mul(20).ok_or_else(size_error)?,
            limits.max_output_bytes,
        )?;
        total = add_size(total, trailer.len(), limits.max_output_bytes)?;
        Ok(Self {
            pages,
            pages_object,
            xref_header,
            trailer,
            xref_offset,
            output_bytes: total,
            object_count,
            max_row_bytes,
        })
    }
}

fn memory_error() -> ToolError {
    ToolError::Failed(
        tr!(
            "没有足够内存生成 PDF",
            "Not enough memory to create the PDF"
        )
        .into(),
    )
}

/// 输入顺序即页面顺序。先精确计划字节数，再分配成品；额外像素缓冲只有一行 BGRA。
pub(crate) fn render(
    images: &[&Image],
    options: PdfOptions,
    limits: PdfLimits,
    cancel: &AtomicBool,
) -> ToolResult<Vec<u8>> {
    check_cancel(cancel)?;
    let plan = PdfPlan::new(images, options, limits, cancel)?;
    check_cancel(cancel)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(plan.output_bytes)
        .map_err(|_| memory_error())?;
    let mut offsets = Vec::new();
    offsets
        .try_reserve_exact(plan.object_count + 1)
        .map_err(|_| memory_error())?;
    offsets.push(0);
    let mut row = Vec::new();
    row.try_reserve_exact(plan.max_row_bytes)
        .map_err(|_| memory_error())?;
    row.resize(plan.max_row_bytes, 0);
    bytes.extend_from_slice(HEADER);
    append_object(&mut bytes, &mut offsets, CATALOG.as_bytes());
    append_object(&mut bytes, &mut offsets, plan.pages_object.as_bytes());
    for (image, record) in images.iter().zip(&plan.pages) {
        check_cancel(cancel)?;
        append_object(&mut bytes, &mut offsets, record.page_object.as_bytes());
        append_object(&mut bytes, &mut offsets, record.content_object.as_bytes());
        append_object(&mut bytes, &mut offsets, record.image_prefix.as_bytes());
        let stream_start = bytes.len();
        let stride = image.width as usize * 4;
        for source in image.bgra.chunks_exact(stride) {
            check_cancel(cancel)?;
            let row = &mut row[..stride];
            row.copy_from_slice(source);
            crate::export::flatten_on_white(row);
            for pixel in row.chunks_exact(4) {
                bytes.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
        }
        if bytes.len() - stream_start != record.rgb_bytes {
            return Err(size_error());
        }
        bytes.extend_from_slice(IMAGE_END);
    }
    if bytes.len() != plan.xref_offset || offsets.len() != plan.object_count + 1 {
        return Err(size_error());
    }
    bytes.extend_from_slice(plan.xref_header.as_bytes());
    bytes.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        check_cancel(cancel)?;
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(plan.trailer.as_bytes());
    if bytes.len() != plan.output_bytes {
        return Err(size_error());
    }
    check_cancel(cancel)?;
    Ok(bytes)
}

fn append_object(bytes: &mut Vec<u8>, offsets: &mut Vec<usize>, object: &[u8]) {
    offsets.push(bytes.len());
    bytes.extend_from_slice(object);
}

/// 创建新 PDF，任何同名文件均不覆盖。调用方须冻结本次来源和参数直到本调用返回。
pub(crate) fn save(
    images: &[&Image],
    options: PdfOptions,
    limits: PdfLimits,
    path: &Path,
    protected_sources: &[PathBuf],
    cancel: &AtomicBool,
) -> ToolResult<SavedPdf> {
    check_cancel(cancel)?;
    validate_destination(path)?;
    if protected_sources.len() > MAX_PAGES {
        return Err(invalid(tr!(
            "原片保护路径过多",
            "Too many protected source paths"
        )));
    }
    for source in protected_sources {
        check_cancel(cancel)?;
        // 源像素已是快照；旧源允许移动，现存占位或链接仍不能参与规范化。
        crate::image_collection::ensure_local_file_or_missing(source)
            .map_err(ToolError::Invalid)?;
        if crate::export::is_same_path(source, path) {
            return Err(invalid(tr!(
                "请另存为新 PDF，保留原片",
                "Save to a new PDF to preserve the source"
            )));
        }
    }
    let bytes = render(images, options, limits, cancel)?;
    check_cancel(cancel)?;
    validate_destination(path)?;
    require_committed(
        crate::export::save_encoded_cancellable(&bytes, path, false, cancel),
        cancel,
    )?;
    Ok(SavedPdf {
        path: path.to_path_buf(),
        pages: images.len(),
        bytes: bytes.len(),
    })
}

fn validate_destination(path: &Path) -> ToolResult<()> {
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        return Err(invalid(tr!("请使用 .pdf 扩展名", "Use a .pdf extension")));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid(tr!("PDF 文件名无效", "Invalid PDF file name")))?;
    if name.encode_utf16().count() > 240 {
        return Err(invalid(tr!("PDF 文件名过长", "PDF file name is too long")));
    }
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| invalid(tr!("PDF 文件名无效", "Invalid PDF file name")))?;
    // 复用 Windows 名称校验；PDF 与 PNG 的扩展名长度相同，不使用返回的文件名。
    crate::save_options::file_name(stem, crate::save_options::SaveFormat::Png)
        .map_err(|error| ToolError::Invalid(error.to_string()))?;
    let absolute =
        std::path::absolute(path).map_err(|error| ToolError::Invalid(error.to_string()))?;
    let parent = absolute
        .parent()
        .ok_or_else(|| invalid(tr!("PDF 保存目录无效", "Invalid PDF output folder")))?;
    crate::image_collection::ensure_local_directory(parent).map_err(ToolError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("rustshot-image-pdf-{}-{stamp}", std::process::id()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            if let Ok(entries) = std::fs::read_dir(&self.0) {
                for entry in entries.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let _ = std::fs::remove_dir(&self.0);
        }
    }

    fn image(width: u32, height: u32, pixels: &[[u8; 4]]) -> Image {
        Image {
            width,
            height,
            bgra: pixels.iter().flatten().copied().collect(),
            readback_ms: 0.,
        }
    }

    fn close(left: f64, right: f64) {
        assert!((left - right).abs() < 0.000001, "{left} != {right}");
    }

    fn find(bytes: &[u8], needle: &[u8]) -> usize {
        bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap()
    }

    #[test]
    fn image_pdf_layout_keeps_explicit_dpi_a4_orientation_and_centered_aspect() {
        let native = page_layout(
            300,
            150,
            PdfOptions {
                page_size: PdfPageSize::ImagePixels { dpi: 150. },
                margin_mm: 0.,
            },
        )
        .unwrap();
        close(native.page_width_pt, 144.);
        close(native.page_height_pt, 72.);
        close(native.image_x_pt, 0.);
        close(native.image_y_pt, 0.);
        let portrait = page_layout(200, 100, PdfOptions::default()).unwrap();
        close(portrait.page_width_pt, 210. * 72. / 25.4);
        close(portrait.image_width_pt / portrait.image_height_pt, 2.);
        close(portrait.image_x_pt, 10. * 72. / 25.4);
        close(
            portrait.image_y_pt * 2. + portrait.image_height_pt,
            portrait.page_height_pt,
        );
        let landscape = page_layout(
            200,
            100,
            PdfOptions {
                page_size: PdfPageSize::A4Landscape,
                ..Default::default()
            },
        )
        .unwrap();
        close(landscape.page_width_pt, portrait.page_height_pt);
        close(landscape.page_height_pt, portrait.page_width_pt);
    }

    #[test]
    fn image_pdf_rejects_nonfinite_margins_dpi_and_page_boundaries() {
        for margin in [f64::NAN, f64::INFINITY, -1., 105.] {
            assert!(page_layout(
                1,
                1,
                PdfOptions {
                    margin_mm: margin,
                    ..Default::default()
                }
            )
            .is_err());
        }
        for dpi in [0., f64::NAN, f64::INFINITY, 9601.] {
            assert!(page_layout(
                100,
                100,
                PdfOptions {
                    page_size: PdfPageSize::ImagePixels { dpi },
                    margin_mm: 0.
                }
            )
            .is_err());
        }
        assert!(page_layout(
            1,
            1,
            PdfOptions {
                page_size: PdfPageSize::ImagePixels { dpi: 96. },
                margin_mm: 0.
            }
        )
        .is_err());
        assert!(page_layout(
            65_535,
            1,
            PdfOptions {
                page_size: PdfPageSize::ImagePixels { dpi: 1. },
                margin_mm: 0.
            }
        )
        .is_err());
        assert!(page_layout(0, 1, PdfOptions::default()).is_err());
    }

    #[test]
    fn image_pdf_rgb_stream_is_binary_exact_top_row_first_and_flattens_alpha() {
        let source = image(
            2,
            2,
            &[
                [0, 10, 13, 255],
                [255, 0, 255, 255],
                [10, 20, 30, 128],
                [0, 0, 0, 0],
            ],
        );
        let before = source.bgra.clone();
        let bytes = render(
            &[&source],
            PdfOptions::default(),
            PdfLimits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(bytes.starts_with(HEADER));
        let prefix = b"/ColorSpace /DeviceRGB /BitsPerComponent 8 /Length 12 >>\nstream\n";
        let start = find(&bytes, prefix) + prefix.len();
        assert_eq!(
            &bytes[start..start + 12],
            &[13, 10, 0, 255, 0, 255, 142, 137, 132, 255, 255, 255]
        );
        assert!(bytes[start + 12..].starts_with(IMAGE_END));
        assert_eq!(source.bgra, before);
    }

    #[test]
    fn image_pdf_xref_offsets_stream_lengths_and_page_order_are_exact() {
        let first = image(1, 1, &[[1, 2, 3, 255]]);
        let second = image(2, 1, &[[4, 5, 6, 255]; 2]);
        let bytes = render(
            &[&first, &second],
            PdfOptions::default(),
            PdfLimits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let xref = find(&bytes, b"xref\n0 9\n");
        let entries = xref + b"xref\n0 9\n".len();
        assert_eq!(&bytes[entries..entries + 20], b"0000000000 65535 f \n");
        for id in 1..9 {
            let entry = &bytes[entries + id * 20..entries + (id + 1) * 20];
            let offset: usize = std::str::from_utf8(&entry[..10]).unwrap().parse().unwrap();
            assert!(bytes[offset..].starts_with(format!("{id} 0 obj\n").as_bytes()));
            assert_eq!(&entry[10..], b" 00000 n \n");
        }
        let startxref = format!("startxref\n{xref}\n%%EOF\n");
        assert!(bytes.ends_with(startxref.as_bytes()));
        assert!(find(&bytes, b"/Kids [3 0 R 6 0 R ] /Count 2") < xref);
        for id in [4, 7] {
            let object = find(&bytes, format!("{id} 0 obj\n<< /Length ").as_bytes());
            let length_start = object + format!("{id} 0 obj\n<< /Length ").len();
            let suffix = find(&bytes[length_start..], b" >>\nstream\n");
            let length: usize = std::str::from_utf8(&bytes[length_start..length_start + suffix])
                .unwrap()
                .parse()
                .unwrap();
            let stream_start = length_start + suffix + b" >>\nstream\n".len();
            assert!(bytes[stream_start + length..].starts_with(b"endstream\nendobj\n"));
        }
    }

    #[test]
    fn image_pdf_budgets_malformed_inputs_and_cancel_are_checked_before_output_allocation() {
        let source = image(1, 1, &[[0; 4]]);
        let cancel = AtomicBool::new(false);
        assert!(render(&[], PdfOptions::default(), PdfLimits::default(), &cancel).is_err());
        let malformed = image(2, 1, &[[0; 4]]);
        assert!(render(
            &[&malformed],
            PdfOptions::default(),
            PdfLimits::default(),
            &cancel
        )
        .is_err());
        for limits in [
            PdfLimits {
                max_total_pixels: 1,
                ..Default::default()
            },
            PdfLimits {
                max_pages: 1,
                ..Default::default()
            },
            PdfLimits {
                max_output_bytes: 100,
                ..Default::default()
            },
        ] {
            assert!(render(&[&source, &source], PdfOptions::default(), limits, &cancel).is_err());
        }
        let bytes = render(
            &[&source],
            PdfOptions::default(),
            PdfLimits::default(),
            &cancel,
        )
        .unwrap();
        assert!(render(
            &[&source],
            PdfOptions::default(),
            PdfLimits {
                max_output_bytes: bytes.len() - 1,
                ..Default::default()
            },
            &cancel
        )
        .is_err());
        assert_eq!(
            render(
                &[&source],
                PdfOptions::default(),
                PdfLimits {
                    max_output_bytes: bytes.len(),
                    ..Default::default()
                },
                &cancel
            )
            .unwrap(),
            bytes
        );
        assert!(matches!(
            render(
                &[&source],
                PdfOptions::default(),
                PdfLimits::default(),
                &AtomicBool::new(true)
            ),
            Err(ToolError::Cancelled)
        ));
    }

    #[test]
    fn image_pdf_save_preserves_sources_existing_outputs_and_cancelled_targets() {
        let directory = TestDirectory::new();
        let source = image(1, 1, &[[10, 20, 30, 128]]);
        let original = directory.0.join("source.pdf");
        std::fs::write(&original, b"keep source").unwrap();
        let cancel = AtomicBool::new(false);
        assert!(save(
            &[&source],
            PdfOptions::default(),
            PdfLimits::default(),
            &original,
            &[original.clone()],
            &cancel
        )
        .is_err());
        assert_eq!(std::fs::read(&original).unwrap(), b"keep source");
        let destination = directory.0.join("result.pdf");
        let moved = directory.0.join("moved.png");
        let saved = save(
            &[&source],
            PdfOptions::default(),
            PdfLimits::default(),
            &destination,
            &[moved],
            &cancel,
        )
        .unwrap();
        let bytes = std::fs::read(&destination).unwrap();
        assert_eq!(saved.pages, 1);
        assert_eq!(saved.bytes, bytes.len());
        assert_eq!(saved.path, destination);
        assert_eq!(
            bytes,
            render(
                &[&source],
                PdfOptions::default(),
                PdfLimits::default(),
                &cancel
            )
            .unwrap()
        );
        assert!(save(
            &[&source],
            PdfOptions::default(),
            PdfLimits::default(),
            &destination,
            &[],
            &cancel
        )
        .is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), bytes);
        let cancelled = directory.0.join("cancelled.pdf");
        assert!(matches!(
            save(
                &[&source],
                PdfOptions::default(),
                PdfLimits::default(),
                &cancelled,
                &[],
                &AtomicBool::new(true)
            ),
            Err(ToolError::Cancelled)
        ));
        assert!(!cancelled.exists());
        assert!(save(
            &[&source],
            PdfOptions::default(),
            PdfLimits::default(),
            &directory.0.join("wrong.png"),
            &[],
            &cancel
        )
        .is_err());
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 2);
    }

    /// 显式生成固定合成图，交由独立 PDF 阅读器核对页序、方向、居中和白底透明。
    #[test]
    #[ignore = "writes synthetic PDF samples for independent reader verification"]
    fn image_pdf_smoke_artifacts() {
        let make_image = |width: u32, height: u32| {
            let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
            for y in 0..height {
                for x in 0..width {
                    let mut color = match (x < width / 2, y < height / 2) {
                        (true, true) => [0, 0, 255, 255],
                        (false, true) => [0, 255, 0, 255],
                        (true, false) => [255, 0, 0, 255],
                        (false, false) => [0, 255, 255, 255],
                    };
                    if x >= width * 3 / 4 {
                        color[3] = 128;
                    }
                    if x < 2 || y < 2 || x >= width - 2 || y >= height - 2 {
                        color = [0, 0, 0, 255];
                    }
                    bgra.extend_from_slice(&color);
                }
            }
            Image {
                width,
                height,
                bgra,
                readback_ms: 0.,
            }
        };
        let wide = make_image(240, 160);
        let tall = make_image(120, 240);
        let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/image-pdf-smoke");
        std::fs::create_dir_all(&directory).unwrap();
        crate::image_collection::ensure_local_directory(&directory).unwrap();
        let cancel = AtomicBool::new(false);
        for (name, options) in [
            ("pages-a4-portrait.pdf", PdfOptions::default()),
            (
                "pages-a4-landscape.pdf",
                PdfOptions {
                    page_size: PdfPageSize::A4Landscape,
                    margin_mm: 20.,
                },
            ),
            (
                "pages-native-96dpi.pdf",
                PdfOptions {
                    page_size: PdfPageSize::ImagePixels { dpi: 96. },
                    margin_mm: 0.,
                },
            ),
        ] {
            let bytes = render(&[&wide, &tall], options, PdfLimits::default(), &cancel).unwrap();
            let path = directory.join(name);
            crate::image_collection::ensure_local_file_or_missing(&path).unwrap();
            // 只允许显式运行该样本测试时替换上述固定的合成输出。
            assert!(crate::export::save_encoded_cancellable(&bytes, &path, true, &cancel).unwrap());
        }
        for (name, source) in [("source-wide.png", &wide), ("source-tall.png", &tall)] {
            let path = directory.join(name);
            crate::image_collection::ensure_local_file_or_missing(&path).unwrap();
            let bytes =
                crate::export::encode_image(source, crate::save_options::SaveEncoding::default())
                    .unwrap();
            assert!(crate::export::save_encoded_cancellable(&bytes, &path, true, &cancel).unwrap());
        }
    }
}
