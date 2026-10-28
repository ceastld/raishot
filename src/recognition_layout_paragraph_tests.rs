//! 段落判定的真实几何回归：行文本与行框取自 ppocr 对合成截图的实际输出
//! （target/qa/ocr-paragraph-merge-20261011）。
use super::super::RecognizedLine;
use super::*;

fn page(rows: &[(&str, u32, u32, u32, u32)]) -> RecognizedText {
    let texts: Vec<&str> = rows.iter().map(|row| row.0).collect();
    let rects: Vec<Rect> = rows
        .iter()
        .map(|&(_, x, y, width, height)| Rect {
            x,
            y,
            width,
            height,
        })
        .collect();
    RecognizedText {
        text: texts.join("\n"),
        regions: rects.clone(),
        words: Vec::new(),
        lines: texts
            .iter()
            .zip(rects)
            .map(|(text, rect)| RecognizedLine {
                graphemes: None,
                text: (*text).into(),
                rect: Some(rect),
                horizontal_direction: HorizontalDirection::LeftToRight,
                geometry: None,
            })
            .collect(),
    }
}

/// 每个段落包含的行数。
fn paragraph_sizes(result: &RecognizedText) -> Vec<usize> {
    let copied = OcrCopyLayout::new(result)
        .copy_mapped(0..result.text.len(), CopyLayoutMode::Auto)
        .unwrap();
    let joined: Vec<usize> = copied
        .spans
        .iter()
        .filter(|span| span.kind == CopySpanKind::LineJoin)
        .map(|span| span.source_range.start)
        .collect();
    let mut sizes = vec![1];
    let mut offset = 0;
    for line in result.text.split('\n').collect::<Vec<_>>().split_last().unwrap().1 {
        offset += line.len();
        if joined.contains(&offset) {
            *sizes.last_mut().unwrap() += 1;
        } else {
            sizes.push(1);
        }
        offset += 1;
    }
    sizes
}

#[test]
fn padded_english_bands_overlap_but_still_join_and_split_at_paragraph_gap() {
    let result = page(&[
        ("Paragraph merging should join wrapped lines of the same paragraph while", 25, 28, 659, 34),
        ("keeping real line breaks such as headings, list items, and the boundary between", 25, 61, 695, 30),
        ("two paragraphs. The last line of a paragraph is usually shorter than the column", 24, 90, 690, 31),
        ("width.", 24, 120, 65, 30),
        ("This second paragraph checks whether a larger vertical gap is treated as a", 26, 166, 644, 31),
        ("boundary. It also contains (parentheses) and if words, which are normal prose.", 24, 197, 682, 30),
    ]);
    assert_eq!(paragraph_sizes(&result), vec![4, 2]);
    let copied = OcrCopyLayout::new(&result)
        .copy(0..result.text.len(), CopyLayoutMode::Auto)
        .unwrap();
    assert!(copied.contains("the column width.\nThis second"));
}

#[test]
fn first_line_indent_separates_paragraphs_without_extra_spacing() {
    let result = page(&[
        ("标题：段落测试", 26, 33, 189, 31),
        ("复制排版独立于原图选字，只替换有保守几何证据的续行分隔符。自动续行只处理", 62, 84, 655, 27),
        ("正向轴对齐文字，缺少投影时保留既有合同，有投影但无效、垂直、倾斜、反向或透视", 25, 114, 692, 27),
        ("时保留原文换行，不猜测段落顺序。", 26, 145, 283, 26),
        ("第二段落用于检查段间距是否被识别为段落边界。截图中的网页正文通常行高为字", 61, 176, 656, 24),
        ("号的一点五倍到一点八倍，段间距更大。", 27, 206, 317, 24),
        ("为了做到这一点，算法不能只看文字内容，还要结合每一行在图片中的位置：行与", 62, 235, 655, 27),
        ("行的间距、左边缘是否缩进、右边缘是否写满，以及上下两行是否属于同一栏。", 25, 265, 626, 27),
    ]);
    assert_eq!(paragraph_sizes(&result), vec![1, 3, 2, 2]);
    let copied = OcrCopyLayout::new(&result)
        .copy(0..result.text.len(), CopyLayoutMode::Auto)
        .unwrap();
    assert!(copied.contains("自动续行只处理正向轴对齐文字"), "中文续行不加空格");
}

#[test]
fn list_items_keep_their_markers_and_join_hanging_continuations() {
    let result = page(&[
        ("安装步骤", 25, 30, 106, 34),
        ("1. 下载便携包并解压到任意目录，目录名可以包含中文或空格，但不要放在需要管理员", 25, 73, 695, 30),
        ("权限的位置。", 45, 104, 104, 25),
        ("2. 运行主程序。", 24, 136, 126, 28),
        ("3. 在托盘菜单中打开设置，按需调整快捷键与保存位置，修改会立即生效并写入配置文", 24, 167, 696, 30),
        ("件。", 42, 194, 41, 33),
        ("• Hanging indents keep the wrapped part of a list item aligned with its text, not with the", 25, 240, 681, 27),
        ("bullet in front of it.", 37, 265, 151, 26),
        ("• Short item.", 25, 296, 105, 26),
        ("• Each item is its own paragraph, even when the previous item fills the whole line.", 24, 325, 629, 27),
    ]);
    assert_eq!(paragraph_sizes(&result), vec![1, 2, 1, 2, 2, 1, 1]);
}

#[test]
fn code_block_lines_stay_separate_between_joined_prose() {
    let result = page(&[
        ("下面的示例演示如何在后台读取图片并输出段落合并结果，命令行会打印识别出的每一行以及", 27, 30, 703, 26),
        ("复制排版后的文本。", 25, 55, 151, 32),
        ("let image = export::read_image(&path)?;", 41, 95, 357, 26),
        ("let result = recognize(image, provider)?;", 40, 118, 377, 29),
        ("let text = result.copy_layout().copy(e..len, Auto)?;", 41, 143, 476, 28),
        ("println!(\"{text}\");", 41, 166, 178, 29),
        ("代码块中的每一行都应当保留原有换行，而代码前后的正文段落内部的软换行应当合并为连续", 25, 212, 706, 25),
        ("文字。", 22, 236, 54, 32),
    ]);
    assert_eq!(paragraph_sizes(&result), vec![2, 1, 1, 1, 1, 2]);
}

#[test]
fn evenly_spaced_short_lines_are_not_treated_as_wrapped_text() {
    let result = page(&[
        ("张三10:21", 24, 31, 97, 28),
        ("明天下午三点开会吗?", 25, 62, 179, 29),
        ("李四10:22", 25, 94, 98, 30),
        ("可以，我把议程发群里", 26, 127, 186, 27),
        ("好的", 23, 157, 48, 31),
    ]);
    assert_eq!(paragraph_sizes(&result), vec![1, 1, 1, 1, 1]);
}

#[test]
fn sub_pixel_skew_from_detection_boxes_is_accepted_but_slant_is_not() {
    use crate::annotation::Point;
    let rows = [
        ("第二段落用于检查段间距是否被识别为段落边界。截图中的网页正文通常行高", 26, 149, 844, 35),
        ("一点五倍到一点八倍，段间距更大，正文仍然需要合并成一段连续的文字。", 29, 186, 840, 31),
    ];
    let mut result = page(&rows);
    // 实测 ppocr 四角：843px 宽度上倾斜 1.2px（约 0.08°）。
    result.lines[0].geometry = Some(TextGeometry {
        quad: [
            Point { x: 26.142, y: 149.541 },
            Point { x: 869.011, y: 150.749 },
            Point { x: 868.959, y: 183.658 },
            Point { x: 26.090, y: 182.450 },
        ],
    });
    assert_eq!(paragraph_sizes(&result), vec![2]);
    // 左右边错切 8px（约 14°）的行保留原换行。
    result.lines[0].geometry = Some(TextGeometry {
        quad: [
            Point { x: 34., y: 149.5 },
            Point { x: 869., y: 149.5 },
            Point { x: 861., y: 183. },
            Point { x: 26., y: 183. },
        ],
    });
    assert_eq!(paragraph_sizes(&result), vec![1, 1]);
}

#[test]
fn bulletless_list_items_split_on_slightly_larger_item_spacing() {
    // 网页列表的圆点未被检测到：项间距只比行距大约 5px，项末以句号结束。
    let result = page(&[
        ("My recommendation", 55, 12, 238, 31),
        ("Choose Codex Plus if you're cost-sensitive, do a lot of small-to-medium tasks, or already", 99, 49, 821, 36),
        ("pay for ChatGPT. It's also the safer pick if hitting limits mid-day would frustrate you.", 99, 79, 782, 36),
        ("Choose Claude Pro if you do large refactors, planning-heavy work, or want skills, hooks", 99, 115, 817, 35),
        ("and MCP. Expect to hit limits on heavy days, and plan to move to Max 5x ($100) f you do.", 97, 144, 826, 36),
        ("At $100, the plans are priced the same, so pick on workflow: Max 5x or Pro 5x.", 101, 182, 730, 31),
        ("Running both is common. Many users plan with Claude and use Codex for follow-ups", 99, 215, 809, 36),
        ("and second opinions.", 97, 248, 209, 31),
    ]);
    assert_eq!(paragraph_sizes(&result), vec![1, 2, 2, 1, 2]);
}
