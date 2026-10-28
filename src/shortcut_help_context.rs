//! 托盘说明与 OCR 完成提示；只描述既有路由，不处理键事件。
use crate::{
    annotation::Tool,
    capture_toolbar::shortcut_hints::{self, Context},
    toolbar_config::AVAILABLE_TOOLS,
};

fn drawing_bindings(context: Context) -> Vec<(Tool, &'static str)> {
    std::iter::once(Tool::Edit)
        .chain(AVAILABLE_TOOLS)
        .filter_map(|tool| shortcut_hints::letter(tool, context).map(|key| (tool, key)))
        .collect()
}

/// 一条快捷键：`keys` 每项是一个组合（如 `Ctrl+Shift+S`，按 `+` 拆成键帽），多项互为替代。
pub(crate) struct HelpRow {
    pub keys: Vec<String>,
    pub action: &'static str,
}

/// 节内的一组条目；`tools` 非空时是绘制工具字母表，按网格显示。
pub(crate) struct HelpGroup {
    pub title: Option<&'static str>,
    pub rows: Vec<HelpRow>,
    pub tools: Vec<(&'static str, &'static str)>,
}

pub(crate) struct HelpSection {
    pub title: &'static str,
    pub groups: Vec<HelpGroup>,
    /// 无法写成按键的补充说明，显示在卡片下方。
    pub note: Option<&'static str>,
}

fn row(keys: &[&str], action: &'static str) -> HelpRow {
    HelpRow {
        keys: keys.iter().map(|key| (*key).to_owned()).collect(),
        action,
    }
}

fn group(title: Option<&'static str>, rows: Vec<HelpRow>) -> HelpGroup {
    HelpGroup {
        title,
        rows,
        tools: Vec::new(),
    }
}

fn tools(title: &'static str, context: Context) -> HelpGroup {
    HelpGroup {
        title: Some(title),
        rows: Vec::new(),
        tools: drawing_bindings(context)
            .into_iter()
            .map(|(tool, key)| (key, tool.label()))
            .collect(),
    }
}

/// 托盘帮助页与截图内帮助共用的说明；只描述现有路由，绘制工具字母读取各上下文的真实绑定。
pub(crate) fn help_sections(capture: &str, pin: &str) -> Vec<HelpSection> {
    let click = tr!("单击", "Click");
    let drag = tr!("拖动", "Drag");
    let wheel = tr!("滚轮", "Wheel");
    let right = tr!("右键", "Right-click");
    let middle = tr!("中键", "Middle-click");
    let space = tr!("空格", "Space");
    vec![
        HelpSection {
            title: tr!("开始", "Getting started"),
            groups: vec![
                group(
                    None,
                    vec![
                        row(&[capture], tr!("区域截图", "Capture an area")),
                        row(
                            &[pin],
                            tr!(
                                "把剪贴板图片或文字贴到屏幕",
                                "Pin the clipboard image or text"
                            ),
                        ),
                    ],
                ),
                group(
                    Some(tr!("托盘图标", "Tray icon")),
                    vec![
                        row(&[tr!("左键", "Left-click")], tr!("区域截图", "Capture an area")),
                        row(
                            &[right],
                            tr!("贴图、历史记录与设置", "Pins, history and settings"),
                        ),
                    ],
                ),
            ],
            note: Some(tr!(
                "全局快捷键可在“设置 ▸ 快捷键”修改或停用。输入文字、打开参数面板或菜单时，按键先交给它们处理。",
                "Change or disable global hotkeys in Settings ▸ Hotkeys. While typing or with a panel or menu open, keys go there first."
            )),
        },
        HelpSection {
            title: tr!("截图", "Capture"),
            groups: vec![
                group(
                    Some(tr!("框选前", "Before selecting")),
                    vec![
                        row(
                            &[click],
                            tr!("采用自动识别的窗口或控件", "Take the detected window or control"),
                        ),
                        row(&[drag], tr!("自定范围", "Select your own area")),
                        row(&["Alt+A"], tr!("当前屏幕", "Current screen")),
                        row(&["Ctrl+Shift+A"], tr!("全部屏幕", "All screens")),
                        row(&["R"], tr!("识别悬停处文字", "Read hovered text")),
                        row(&["C"], tr!("复制放大镜处颜色", "Copy the magnifier color")),
                        row(
                            &[tr!("方向键", "Arrows"), "WASD"],
                            tr!("微调指针", "Nudge the pointer"),
                        ),
                        row(&["F1"], tr!("显示／隐藏提示卡", "Show/hide the hint card")),
                    ],
                ),
                group(
                    Some(tr!("框选后", "After selecting")),
                    vec![
                        row(&["Enter", "✓"], tr!("执行完成动作", "Run the finish action")),
                        row(&["Ctrl+C"], tr!("复制", "Copy")),
                        row(&["Ctrl+S"], tr!("保存", "Save")),
                        row(&["Ctrl+Shift+S"], tr!("快速保存", "Quick save")),
                        row(&[middle], tr!("贴图", "Pin")),
                        row(&["R"], tr!("文字识别", "Recognize text")),
                        row(&["T"], tr!("识别并翻译", "Recognize and translate")),
                        row(&["Y"], tr!("图片翻译", "Translate image")),
                        row(&["L"], tr!("长截图", "Long capture")),
                        row(&["G"], tr!("录屏", "Record")),
                        row(
                            &["F5"],
                            tr!(
                                "刷新底图，保留选区和标注",
                                "Refresh the desktop, keep area and annotations"
                            ),
                        ),
                    ],
                ),
                group(
                    Some(tr!("通用", "Anytime")),
                    vec![
                        row(&[space], tr!("收起／显示工具栏", "Hide/show the toolbar")),
                        row(&[right], tr!("重选", "Reselect")),
                        row(
                            &["Esc"],
                            tr!(
                                "先退出当前操作，再退出截图",
                                "Leave the current action, then exit"
                            ),
                        ),
                        row(&["V"], tr!("编辑对象", "Edit objects")),
                        row(
                            &["0"],
                            tr!(
                                "在编辑对象与调整选区之间切换",
                                "Switch between objects and the area"
                            ),
                        ),
                        row(&["Ctrl+A"], tr!("编辑时全选标注", "Select all annotations")),
                        row(&["Shift+F1"], tr!("截图内帮助", "In-capture help")),
                    ],
                ),
                tools(
                    tr!("绘制工具字母", "Drawing tool letters"),
                    Context::Capture { smart_mosaic: true },
                ),
            ],
            note: None,
        },
        HelpSection {
            title: tr!("贴图", "Pins"),
            groups: vec![
                group(
                    Some(tr!("窗口", "Window")),
                    vec![
                        row(&[drag], tr!("移动", "Move")),
                        row(&[wheel], tr!("缩放", "Zoom")),
                        row(&[tr!("Shift+滚轮", "Shift+Wheel")], tr!("精细缩放", "Fine zoom")),
                        row(&[tr!("Ctrl+滚轮", "Ctrl+Wheel")], tr!("调透明度", "Opacity")),
                        row(&[middle], tr!("恢复 100%", "Reset to 100%")),
                        row(&[tr!("双击", "Double-click"), "Tab"], tr!("缩略", "Thumbnail")),
                        row(&[space], tr!("展开／收起绘制栏", "Open/close the drawing bar")),
                        row(&[right], tr!("菜单（列出全部操作）", "Menu with every action")),
                        row(
                            &["Esc"],
                            tr!(
                                "先取消当前操作，无操作时关闭贴图",
                                "Cancel the current action, or close when idle"
                            ),
                        ),
                    ],
                ),
                group(
                    Some(tr!("复制与保存", "Copy and save")),
                    vec![
                        row(&["Ctrl+C"], tr!("复制图片或所选文字", "Copy image or selected text")),
                        row(
                            &["Ctrl+Shift+C"],
                            tr!("复制全部识别文字", "Copy all recognized text"),
                        ),
                        row(&["Ctrl+S"], tr!("保存", "Save")),
                        row(&["Ctrl+W"], tr!("关闭", "Close")),
                    ],
                ),
                group(
                    Some(tr!(
                        "绘制栏收起或贴图锁定时",
                        "Drawing bar closed or pin locked"
                    )),
                    vec![
                        row(&["E"], tr!("选择文字", "Select text")),
                        row(&["H"], tr!("显示／隐藏标注", "Show/hide annotations")),
                        row(&["O"], tr!("重新识别", "Recognize again")),
                        row(&["F2"], tr!("设置标题", "Set a title")),
                        row(&["1–8"], tr!("旋转、翻转与调色", "Rotate, flip and adjust colors")),
                    ],
                ),
                tools(
                    tr!("绘制栏展开时按字母选工具", "Drawing bar open: letters pick tools"),
                    Context::Pin,
                ),
            ],
            note: Some(tr!(
                "关闭的贴图可从托盘或主界面“贴图”页恢复。",
                "Restore closed pins from the tray or the Pins page."
            )),
        },
        HelpSection {
            title: tr!("长截图（框选后按 L）", "Long capture (L after selecting)"),
            groups: vec![group(
                None,
                vec![
                    row(
                        &[wheel],
                        tr!("在目标页面滚动即可拼接", "Scroll the target page to stitch"),
                    ),
                    row(&[space], tr!("暂停／继续", "Pause/resume")),
                    row(&["Enter", "✓"], tr!("复制长图并结束", "Copy and finish")),
                    row(&["Ctrl+S"], tr!("保存", "Save")),
                    row(
                        &["Esc"],
                        tr!(
                            "先取消正在进行的拖动，再退出",
                            "Cancel an ongoing drag, then exit"
                        ),
                    ),
                ],
            )],
            note: Some(tr!(
                "也可以点“自动滚动”；“裁剪”打开调整窗，删段、裁边后再复制、保存、贴图或标注；采集中拖“移动选区”可沿滚动方向移动。",
                "Or click Auto-scroll. Crop opens the adjust window to cut sections or trim edges before copying, saving, pinning or annotating. While capturing, drag Move area along the scroll direction."
            )),
        },
        HelpSection {
            title: tr!("录屏（框选后按 G）", "Recording (G after selecting)"),
            groups: vec![
                group(
                    Some(tr!("录制中", "While recording")),
                    vec![
                        row(&["F9"], tr!("开始／停止", "Start/stop")),
                        row(&["F10", space], tr!("暂停／继续", "Pause/resume")),
                        row(
                            &["Esc"],
                            tr!("停止（倒计时中为取消）", "Stop (cancels during countdown)"),
                        ),
                        row(&["Ctrl+Z"], tr!("撤销屏幕标注", "Undo markup")),
                    ],
                ),
                group(
                    Some(tr!("停止后预览", "Preview after stopping")),
                    vec![
                        row(&[space], tr!("播放", "Play")),
                        row(&["Ctrl+C"], tr!("复制文件", "Copy the file")),
                        row(&["Ctrl+S"], tr!("另存", "Save as")),
                        row(&["Enter"], tr!("完成", "Finish")),
                    ],
                ),
            ],
            note: Some(tr!(
                "录制中点“标注”可在屏幕上画，Esc 收起标注。预览可转 GIF 或重录；成片在主界面“历史”页。",
                "While recording, click Markup to draw on screen; Esc closes markup. The preview can convert to GIF or record again; recordings are on the History page."
            )),
        },
        HelpSection {
            title: tr!("标注与文字", "Annotations and text"),
            groups: vec![
                group(
                    None,
                    vec![
                        row(
                            &["1–9"],
                            tr!(
                                "按工具栏顺序选工具，再按回到编辑",
                                "Pick tools in toolbar order; again for Edit"
                            ),
                        ),
                        row(&["Ctrl+Z"], tr!("撤销", "Undo")),
                        row(&["Ctrl+Y"], tr!("重做", "Redo")),
                        row(&["Delete"], tr!("删除", "Delete")),
                        row(&["Ctrl+D"], tr!("复制对象", "Duplicate")),
                        row(&[tr!("Ctrl+单击", "Ctrl+Click")], tr!("多选", "Multi-select")),
                    ],
                ),
                group(
                    Some(tr!("文字", "Text")),
                    vec![
                        row(&["Enter"], tr!("完成", "Finish")),
                        row(&["Shift+Enter"], tr!("换行", "New line")),
                        row(&["F2"], tr!("重新编辑", "Edit again")),
                    ],
                ),
            ],
            note: Some(tr!(
                "悬停工具栏按钮可看数字；截图里的工具组按同一键轮换。",
                "Hover a toolbar button to see its number; in captures the same key cycles a tool group."
            )),
        },
    ]
}

/// 纯文本形式，供截图内帮助逐行显示。
pub(crate) fn help_text(version: &str, capture: &str, pin: &str) -> String {
    let mut text = format!("{} {version}", crate::brand::NAME);
    for section in help_sections(capture, pin) {
        text.push_str("\n\n");
        text.push_str(section.title);
        for group in section.groups {
            if let Some(title) = group.title {
                text.push('\n');
                text.push_str(title);
                text.push_str(tr!("：", ":"));
            }
            for row in group.rows {
                text.push('\n');
                text.push_str(&row.keys.join(" / "));
                text.push_str("  ");
                text.push_str(row.action);
            }
            for line in group.tools.chunks(4) {
                text.push('\n');
                let line = line
                    .iter()
                    .map(|(key, label)| format!("{key} {label}"))
                    .collect::<Vec<_>>();
                text.push_str(&line.join(" / "));
            }
        }
        if let Some(note) = section.note {
            text.push('\n');
            text.push_str(note);
        }
    }
    text
}

/// 专用 Esc 选字退出先于绘制工具路由；菜单、参数或输入仍先消费。
/// E 在绘制栏展开时会改选工具，故完成提示不把 E 作为退出入口。
pub(crate) fn pin_ocr_selection_hint() -> &'static str {
    tr!(
        "拖选文字，Ctrl+C 复制；Esc 退出选字（菜单、输入优先）",
        "Drag to select, Ctrl+C copies; Esc exits selection (menus/input first)"
    )
}

#[cfg(test)]
#[path = "shortcut_help_context_tests.rs"]
mod tests;
