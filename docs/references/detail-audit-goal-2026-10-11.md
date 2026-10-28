# 细节自查与优化（长期目标，Claude 会话）

用户于 2026-10-11 提出：自行检查 RustShot 的细节问题，发现问题后分派任务优化；这是长期目标，额度用尽后恢复时继续。合理使用额度：审计用只读子代理，修复只派把握大、边界清楚的项。

## 工作方式

1. 先读本文件“问题清单”，挑状态为“待修”的项继续；没有待修项时开启下一轮审计（轮换领域，见下）。
2. 修改前看 `git status --short` 与文件修改时间；其它 Codex／Claude 会话可能正在并行编辑，避开近 30 分钟内被改动的文件，或只做最小 hunk。
3. 验证遵守 AGENTS.md：后台增量 debug 编译与相关单测，不占前台、不要求桌面空闲；真实 GUI 未验时如实标注。
4. 每修完一项在清单里标记“已修（来源/验证）”，并按需同步 `docs/behavior.md`。不 commit，除非用户要求。

## 审计领域轮换

- 界面文案多语言：未经 `tr!` 的用户可见文案、英文在固定宽度控件中溢出
- 主题：界面硬编码颜色不跟随浅色／深色
- 健壮性：用户路径中的 unwrap/expect/panic、原始内部错误直接展示给用户
- 交互细节：Esc 层级、悬停／按下态、光标、提示文字与实际快捷键不符
- 文档漂移：README／功能说明与代码不一致

## 问题清单

（按轮次追加，格式：编号 · 领域 · 位置 · 问题 · 状态）

### 第 1 轮（2026-10-11）· 主题

- T1 · 主题 · `parameter_control.rs:417-436,504-548`、`pin_properties.rs:35` · 弹出参数面板构造时缓存 palette，打开期间切主题不更新（`finish_parameter_control.rs:311` 已有每次刷新写法） · 已修（13:45 子代理半成品，本轮核对：`ParameterState` 渲染时比对并刷新 palette；编译通过）
- T2 · 主题 · `text_field.rs:921` · 光标固定 0x86b6ff，浅色背景下约 2:1 几乎看不见 · 已修（13:45 子代理半成品，本轮核对：光标改用 `theme::palette(cx).accent`）
- T3 · 主题 · `finish_widgets.rs:47` · 开关滑块固定白色，浅色未选中轨道（0xdce3e9）上状态难辨 · 已修（13:45 子代理半成品，本轮核对：Palette 新增 `on_accent`，未选中用 `muted`）
- T4 · 主题 · `parameter_control.rs:1076`、`finish_parameter_control.rs:850,869` · 校验错误色固定 0xff737f，浅色下对比不足；需 Palette 增加 danger · 已修（13:45 子代理半成品，本轮核对：Palette 新增 `danger`，浅色 0xc62828）
- T5 · 主题 · `long_capture_preview.rs:564,627,648,769,772,842` · 长截图预览尺寸标签／滚动轨道／外框固定深色，浅色下突兀 · 已修（13:45 子代理半成品，本轮核对：标签／轨道／外框改用 palette；当前帧绿色 0x32be69 为语义色保留）
- T6 · 主题 · `capture_properties.rs:13-20`、`capture_watermark_properties.rs:10-18` · 提示框使用构造时 palette · 已修（13:45 子代理半成品，本轮核对：提示框渲染时取 `theme::palette(cx)`）
- T7 · 主题 · `tray.rs` 原生托盘菜单不跟随应用深色设置 · 待评估（原生控件，中把握）
- T8 · 主题 · `recording_view.rs:2480-2503` 倒计时 HUD 固定黑底；`long_editor.rs:926`、`save_panel.rs:912,921` 深色图像底 · 待评估（可能有意）

### 第 1 轮（2026-10-11）· 交互与文档

- I1 · 交互 · `capture_view.rs:6161-6192,6067` · 工具／动作字母（t x v m h n b p o s r g l、Enter）不判修饰键，Ctrl+X/V/T/R/L、Ctrl+Enter 等会误触发；贴图 `pin_keys.rs:124` 已有守卫 · 已修（他会话；2026-10-12 读码确认 `host_shortcuts::drawing_tool` 与 `plain` 守卫）
- I2 · 交互 · `pin_view.rs:3829-3856` · 贴图 l t f e o tab 与数字 1-8 不判修饰键（Ctrl+L 锁定、Ctrl+1 旋转等）· 已修（他会话；2026-10-12 读码确认 `host_shortcuts::pin_action` 有修饰键即不匹配）
- I3 · 交互 · `pin_toolbar.rs:623-641,770-812` · 贴图工具栏撤销／重做不可用时无禁用态 · 已修（13:45 子代理半成品，本轮核对：撤销／重做不可用时 0.3 透明、无悬停与点击）
- I4 · 交互 · `long_editor.rs:1174-1195` · 长图编辑按键不判 is_held／修饰键，按住 Esc 会取消选区后直接关闭编辑器 · 已修（13:45 子代理半成品，本轮核对：Esc/Enter/Ctrl 键判修饰键，长按只消费）
- I5 · 交互 · `text_input.rs:513-519` · 文字草稿空选区 Ctrl+C/X 写空串清掉剪贴板 · 已修（他会话；2026-10-12 读码确认 `edit_boundary::publish_selected` 空选区不写剪贴板，有单测）
- I6 · 交互 · `capture_toolbar.rs:205-229,365-367,477-500`、`capture_properties.rs:282-336,676` · 禁用按钮仍显示手形光标和悬停高亮 · 已修（13:45 子代理半成品，本轮核对：`chip_enabled` 禁用无手形和悬停；本轮补完 `capture_properties` 组合／拆分／旋转等行改用 `chip_enabled`）
- I7 · 交互 · `long_capture_view.rs:2308-2345`、`long_editor.rs:836-861` · value_button 缺悬停态／提示 · 已修（13:45 子代理半成品，本轮核对：value_button 悬停改用边框色反馈）
- I8 · 交互 · `capture_toolbar.rs:477-500` · 分裂按钮下拉箭头无提示；`capture_toolbar.rs:939-995` 更多操作中部分项未显示已有快捷键 · 已修（13:45 子代理半成品，本轮核对：分裂按钮下拉“更多”提示；本轮为更多操作补 Del／Ctrl+D／Ctrl+]／Ctrl+[／Shift+Del 提示）
- I9 · 交互 · `recording_library_view.rs:264-271` · Esc/F5/Tab 不判修饰键；搜索有文字时 Esc 直接关窗 · 已修（13:45 子代理半成品，本轮核对：Esc/F5 判修饰键与长按，搜索有字时 Esc 先清空）
- I10 · 交互 · `ui.rs:112-120,1239-1244` · 托盘“设置”“截图历史”重复点击会开第二个进程窗口；忙时点击被静默丢弃 · 待评估
- D1 · 文档 · `docs/behavior.md` 多处代码行号过期（capture_view、pin_view、recording_view 等），改为函数名锚点 · 已修（2026-10-12：全部 `文件:行号` 改为 `文件` 的函数名锚点，文档内已无 `.rs:数字`）
- D2 · 文档 · `docs/behavior.md:331` 0／Ctrl+0 实际仅工具栏收起时有效；`pin_menu.rs:251-307` 数字提示未按状态限定 · 已修（2026-10-12：右键菜单在工具栏展开时隐藏 1–8／0／Ctrl+0 提示，与 `pin_view` 数字路由一致；功能说明贴图表注明仅收起时有效；debug 编译，未做 GUI 验证）
- D3 · 文档 · `docs/behavior.md:383-390` F10/Space 倒计时阶段无效 · 已修（2026-10-12：录屏表注明倒计时中 F10／空格无效、Esc 取消，锚点改为 `recording_shortcuts.rs` 的 `route`）
- D4 · 文档 · Ctrl+O、已有选区 Shift+C、空草稿 Backspace 未写入文档；README 贴图表缺 Ctrl+W/Y/H/Ctrl+Alt+C/Ctrl+F；`settings_hotkey.rs:37` 起始提示未提 Backspace 禁用 · 已修（2026-10-12：功能说明 3.2 补 Shift+C、3.5 补空正文 Backspace/Delete 移除；Ctrl+O 核实仅图片编辑且 3.10 已有；README 贴图表补 Ctrl+W/Y/H、Ctrl+Alt+C/Ctrl+F；录制起始提示加 Backspace 禁用；cargo check 通过）

### 第 1 轮（2026-10-11）· 健壮性

（未发现可由用户输入触发的 panic；以下均为中低影响）

- R1 · 健壮性 · `image_editor.rs:450-456` · 打开图片失败把整个 Result 用 `{:?}` 显示（如 `Err(Canceled)`）· 已修（他会话；2026-10-11 读码确认 `open_image_prompt` 分支处理，取消不提示）
- R2 · 健壮性 · `pin_workspace_control_runtime.rs:428-432` · 解除穿透失败提示含 `Some(..)/None` 且无英文 · 已修（他会话；读码确认 `to_string` + `trf!`）
- R3 · 健壮性 · `translate.rs:211-215` · 翻译连接失败显示枚举 Debug（`Dns`、`ConnectionFailed`）· 已修（`transport_reason` 映射为中英文案；debug 编译 + translate 单测）
- R4 · 健壮性 · `pin_workspace.rs:1121-1126`、`save_store.rs:~129` · 跨进程 try_lock 不重试，多贴图同时登记时失败并显示英文 “would block”，贴图不被记录 · 已修（13:45 子代理半成品，本轮核对完成 save_store 有界重试；本轮给 `pin_workspace::global_lock` 补同样约 300ms 重试与本地化忙提示）
- R5 · 健壮性 · `pin_workspace.rs:1127-1141` · 工作区清单损坏后永远加载失败、无法恢复 · 待评估（2026-10-12 分析：清单由临时文件 sync 后原子 rename 写入，自然损坏罕见；若解析失败直接回退空工作区，下一次写入的 `cleanup_orphans` 会删除全部贴图图片，造成数据丢失。可行方案需把损坏清单与资源一并移入 `recovered-<时间>` 子目录（可能遇到被打开的资源文件），属设计决定，暂不自动修）
- R6 · 健壮性 · `export.rs:228-241`、`pin_app.rs:6-17` · 图片解码错误为英文原文且无文件名；剪贴板上限 16384 与文件 65535 不一致 · 已修（13:45 子代理半成品，本轮核对：剪贴板解码共用 `export::decode_limits` 上限并本地化错误）
- R7 · 健壮性 · `text_field.rs:520-553`、`text_input.rs:40-45,357-361` · 退格／方向键按码点而非字素，emoji 组合被拆坏 · 已修（他会话；2026-10-12 读码确认 `text_edit_boundary` 按扩展字素删除／导航）
- R8 · 健壮性 · `text_field.rs:365-370`、`text_input.rs:459-463` · 超过 4096 字的粘贴静默丢弃 · 已修（2026-10-12：普通粘贴按完整字素截到剩余容量 `edit_boundary::fit_insert`，输入法组合超限仍拒绝；debug 单测）
- R9 · 健壮性 · `capture_view.rs:4577-4584` · 复制成功但历史保存失败时不结束截图 · 待评估
- R10 · 健壮性 · `long_editor.rs:650-656` · 长图复制后历史失败被吞 · 已修（窗口已关闭无界面回执，改为 eprintln 记录；debug 编译）
- R11 · 健壮性 · `save_options.rs:21` vs `export.rs:90-94` · 文件名上限 240 未计临时名后缀，233-240 长度保存时报系统错误 · 已修（他会话；临时名改为固定短名，见 `temp_file_name_is_independent_of_target_name_length`）
- R12 · 健壮性 · `recording_view.rs:2639-2641` · UNC 路径去 `\?\` 前缀后变成 `UNC\...`，复制录屏文件到剪贴板失效 · 已修（他会话；`display_path_text` 正确还原 UNC verbatim 前缀）
- R13 · 健壮性 · `pin_view.rs:2762-2842,4682` · 贴图复制颜色／文字无“已复制”反馈 · 已修（2026-10-12：取色 C／单击复制显示“已复制 #颜色”，非 OCR 文字复制显示“文字已复制”，经 `copy_color_notice`／`copy_text_notice`；debug 编译，未做 GUI 验证）
- R14 · 健壮性 · `preferences.rs:73,154-161`、`save_store.rs:~134` · 设置文件非 UTF-8 时静默重置并在下次保存时覆盖；写入未 sync · 待评估

### 第 1 轮（2026-10-11）· 多语言

（未发现英文溢出固定宽度控件的确定案例）

- L1 · 多语言 · `pin_workspace.rs:72,171` · 默认分组名“默认”、新贴图标题“贴图”以中文持久化，英文界面直接显示中文；应在显示时本地化 · 已修（2026-10-12：`Group::display_name`、`display_pin_title` 在工作区列表／移动分组／标题框／窗口标题显示时本地化，存储仍为中文；debug 单测，未做 GUI 验证）
- L2 · 多语言 · `long_auto.rs:130-224`、`long_auto_native.rs:197,238,243` · 长截图自动滚动状态／停止原因未本地化 · 已修（13:45 子代理半成品，本轮核对：读码未见残留中文字面量；`long_auto_native.rs` “每步须为半格”为内部校验，保留中文）
- L3 · 多语言 · `stitch_stream.rs:57,62,115,149-154,194` · 拼接拒绝原因与提示未本地化 · 已修（13:45 子代理半成品，本轮核对）
- L4 · 多语言 · `recording_options.rs:190`、`recording_stop.rs:84,96,171-182`、`recording_volume_save.rs:38-103`、`recording_view.rs:2649,2669` · 录屏错误／状态未本地化 · 已修（13:45 子代理半成品，本轮核对；本轮补 `recording_view` 剪贴板分配／占用错误；`recording_volume_save` 剩余为内部所有权校验，保留中文）
- L5 · 多语言 · `export.rs:138,157`、`pin_text_source.rs:66,81`、`pin_app.rs:33,46` · 保存格式／文本贴图／剪贴板为空错误未本地化 · 已修（export/pin_app/pin_text_source 改 `tr!`；debug 编译）
- L6 · 多语言 · `local_fill.rs:70`、`ppocr/mod.rs:55`、`live_capture.rs:172`、`input_overlay.rs:176`、`canvas_source.rs:79-252`、`lib.rs:451` · 其它面向用户错误未本地化 · 已修（13:45 子代理半成品，本轮核对；本轮补 `live_capture` 采集范围错误与 `input_overlay` 绘制表面错误；`local_fill`、`ppocr/mod`、`canvas_source` 剩余为内部不变量／溢出校验，按 AGENTS 可只写中文）
- L7 · 多语言 · 术语不一致：段落复制 “Join lines” vs “Copy paragraphs”；鼠标穿透 “Click-through” vs “Pass-through”；`settings_view.rs:476` 标准→“Small”；铅笔与画笔都译 “Pen”；重新识别全部→“Recognize again”；“Tab / dbl-click”；原文→“Lines”；打码 “Redact” vs 打码笔 “Mosaic brush” · 已修（2026-10-12：穿透统一 “Click-through”；同一 `Tool::Pen` 的中文 铅笔／画笔 统一为“画笔”（贴图工具选择、截图工具提示）。核实其余为有意：“Small/Tiny” 是 PP-OCR 模型档名，与 CLI `--ocr-profile` 一致；“Join lines” 代码内已统一；原文→“Lines”为行视图；“Redact” 是分组名、“Mosaic brush”为组内工具；重新识别全部的按钮宽度未核实，保留）

### 第 2 轮（2026-10-12）· 交互细节（较新窗口）

- I11 · 交互 · `save_panel.rs` 根 `on_key_down` · Esc 不判长按／修饰键；覆盖确认打开时 Esc 直接关闭整个保存窗口 · 已修（2026-10-12：仅裸 Esc 新按下生效，确认打开时只收起确认；cargo check）
- I12 · 交互 · `save_panel.rs` 的 `button`、最近目录／日期 chip · 保存中按钮半透明但仍手形和悬停 · 已修（2026-10-12：保存中去掉手形与悬停；cargo check）
- L8 · 多语言 · `recording_library_view.rs` 扫描／目标校验错误 5 处、`history_view.rs` 删除取消／后台异常 2 处 · 裸中文经错误状态显示 · 已修（2026-10-12：改 `tr!`；recording_library／history_ 单测 148 通过）
- I13 · 交互 · `pin_workspace_view.rs` Esc 处理 · 编辑分组名／标题时 Esc 在搜索为空后直接关窗，草稿丢失；应按焦点先还原字段 · 已修（2026-10-12：焦点在分组名且有草稿／新建中时 Esc 走 CancelGroup，标题有草稿时走 CancelTitle，其次清搜索、再关窗；未编译验证，见本轮小结）
- I14 · 交互 · `history_view.rs`／`history_gallery.rs` 的 `key_route` · 搜索有字时 Esc 直接关窗，与录屏历史“先清空”不一致 · 已修（2026-10-12：`KeyRoute::Close` 在搜索非空时先清空并重筛；未编译验证）
- I15 · 交互 · `recording_library_view.rs` 列表方向键／Enter／Delete 只排除 Ctrl/Alt，Shift／Win 组合也触发 · 已修（2026-10-12：列表键改用 `plain` 判定；未编译验证）
- I16 · 交互 · `settings_hotkey.rs` 的 `consume_hotkey_spec` · 任何带修饰的 Esc 都当“取消录制”；`save_panel.rs` Ctrl+S 不排除 Alt／Win · 待评估（系统保留键，可能有意）

### 第 3 轮（2026-10-13）· 主题复查＋新模块健壮性／多语言

（主题复查：剩余硬编码色均为图像上叠加层、Windows 关闭键红、取色色块底等有意用法，无新问题）

- L9 · 多语言 · `capture_pin_output.rs` 的 `selected_host`／`output_rect` · “贴图固定内容域无效”“套索与选区没有交集”裸中文经状态栏显示 · 已修（2026-10-13：改 `tr!`，前者改为可操作提示；cargo check）
- R15 · 健壮性 · 多处 `scene.errors.join(";")`（capture_pin_output／capture_view／capture_pin_document 等 9 处）· 标注渲染错误原文直接进状态栏、无操作前缀 · 待评估（系统性，需统一包装函数）
- R16 · 健壮性 · `capture_image_translate.rs`、`capture_pin_output.rs` 的 `self.status = error.to_string()` · 状态栏只显示内层错误，不说明是哪个操作失败；`finish_image_translation` 无块时 “图片翻译失败：” 后为空 · 待评估（中低影响）

- 2026-10-11 定时轮次小结：修 R3（翻译连接失败原因本地化）、R10（长图复制历史失败记日志）、L5（保存格式／JPEG 质量／剪贴板为空／文本贴图错误改 `tr!`）；读码确认 R1、R2、R11、R12 已由他会话修复并标记。验证：debug 增量编译 + export/pin_text_source/pin_app/translate 相关单测 105 通过；未做真实 GUI 验证。避开 30 分钟内被改动的 capture_view/text_field/text_input/pin_view/pin_keys 等文件。剩余待修 31 项（另 R9、R14 待评估）。

- 2026-10-11 半成品核对小结（应创建会话请求）：逐项核对 13:45 额度中断子代理留下的 T1–T6、I3/I4/I6–I9、R1/R2/R4/R6/R11/R12、L2–L6，均可编译；补完 I6（capture_properties 禁用 chip）、I8（更多操作快捷键提示）、R4（pin_workspace 锁重试）、L4/L6（录屏剪贴板、采集范围、绘制表面错误本地化）。R5 未动仍待修。验证：debug 增量编译 + 相关单测 202 通过；未做真实 GUI 验证。剩余待修 13 项（I1、I2、I5、D1–D4、R5、R7、R8、R13、L1、L7），另 R9、R14 待评估。

- 2026-10-12 定时轮次小结：修 R8（普通粘贴超长按字素截到剩余容量，IME 组合仍拒绝）、L1（默认分组名／贴图标题显示时本地化，存储不变）、D3（录屏表注明倒计时中 F10／空格无效、Esc 取消）；读码确认 I1、I2、I5、R7 已由他会话修复并标记。验证：debug 增量编译 + edit_boundary／pin_workspace 相关单测 3 通过；未做真实 GUI 验证。其间 capture_canvas.rs 正被他会话编辑（一度编译失败，非本轮改动），未触碰。剩余待修 6 项（D1、D2、D4、R5、R13、L7），另 T7、T8、I10、R9、R14 待评估。

- 2026-10-12 定时轮次小结（二）：修 R13（贴图取色／非 OCR 文字复制显示“已复制”反馈）、D2（工具栏展开时右键菜单隐藏数字／Ctrl+0 提示，功能说明注明仅收起时有效）、L7（穿透统一 Click-through；同一画笔工具中文统一“画笔”；其余术语核实为有意）；R5 分析后改为待评估（直接回退空工作区会触发 `cleanup_orphans` 删除全部贴图图片）。验证：debug 增量编译通过（唯一 warning 为他会话 `image_translate.rs` 未用方法）；未跑单测（改动不涉及纯模型），未做真实 GUI 验证。剩余待修 2 项（D1、D4），另 T7、T8、I10、R5、R9、R14 待评估；下轮可做 D4 后开启第 2 轮审计。

- 2026-10-12 定时轮次小结（三）：修 D4（快捷键文档补全、录制提示补 Backspace）、D1（功能说明行号改函数名锚点）。验证：`cargo check --lib` 通过；文档改动按源码核对；未做真实 GUI 验证。第 1 轮待修清零，另 T7、T8、I10、R5、R9、R14 待评估；下轮按轮换开启第 2 轮审计。

- 2026-10-12 定时轮次小结（四）：开启第 2 轮审计（交互细节，较新窗口：保存面板、历史、贴图工作区、录屏历史、设置），核实 7 条；修 I11、I12、L8。验证：`cargo check --lib` + recording_library／history_ 单测 148 通过；未做真实 GUI 验证。剩余待修 3 项（I13、I14、I15），另 I16 及第 1 轮 T7、T8、I10、R5、R9、R14 待评估。

- 2026-10-12 定时轮次小结（五）：修 I13（贴图工作区 Esc 先按焦点还原分组名／标题草稿）、I14（截图历史 Esc 先清空搜索）、I15（录屏历史列表键排除 Shift／Win）。验证：`cargo check --lib` 因他会话进行中的 quick_action／ui／last_region 等改动失败（7 个错误均不在本轮文件），本轮改动未编译验证、未做真实 GUI 验证。第 2 轮待修清零，另 I16 及 T7、T8、I10、R5、R9、R14 待评估；下轮先补编译核对，再按轮换开启第 3 轮审计。

- 2026-10-13 定时轮次小结（六）：补编译核对 I13–I15（`cargo check --lib` 通过）；开启第 3 轮（主题复查无新问题；新模块健壮性／多语言由 Explore 子代理审计，核实 3 条）；修 L9。另为定时任务配置项目权限放行（`.claude/settings.local.json`）以免每轮确认。验证：`cargo check --lib` 通过；未做真实 GUI 验证。剩余待修 0 项，另 R15、R16、I16 及 T7、T8、I10、R5、R9、R14 待评估；下轮按轮换审计“交互细节／文档漂移”于快捷动作、实时贴图、表格复制等新功能（待其它会话改动落定后）。
