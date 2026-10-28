# 截图基础体验差距审计（2026-10-12）

本页记录 2026-10-12 对截图基础能力的历史对照审计，保留项目能力差距与实施建议。当前开发依据是公开的[行为清单](../behavior.md)、[当前状态](../status.md)和仓库源码，不需要访问历史对照工程。“截图后快捷组合操作”（热键直达 OCR 复制等）与表格识别另有专题，本文只在相关处注明“交给快捷组合设计”。后文源码行号对应当时版本，定位当前实现时应按函数或任务关键词搜索。

## 结论

1. **大部分基础能力已经有了，而且多数比截图Pro源码更全**：Shift 框 1:1、Shift 角拖锁比例、方向键/Alt+方向键微调、Ctrl+A／Alt+A／Ctrl+Shift+A 选屏、反引号切换光标、放大镜取色（C 复制、Shift 切格式、P 切坐标）、快速保存（Ctrl+Shift+S + 完整保存规则）、贴图透明度／穿透／锁定／分组／关闭恢复／拖出、Ctrl+Shift+Z＝重做、Shift+Delete 清空、F9/F10 录屏都已实现。
2. **P0 缺口集中在“选得准”和“出得快”**：没有选区宽高/坐标输入；键盘只能改右、下两条边（Alt+方向），不能逐边收缩/扩展；没有“上次区域／最近选区”；确认（Enter／✓／双击）固定为复制，不能设成贴图或快速保存。
3. **快捷键底座是最大的 P1 债**：只有托盘里的 MessageBox 帮助（`tray.rs:255`），截图内没有 `?` 帮助层；会话内快捷键不能自定义；全局热键只有截图和剪贴板贴图两个（`hotkey.rs:11`）。
4. **贴图缺口是 P1/P2 小项**：标题只能在工作区里设，贴图窗口上看不到；“缩略收纳”只是缩到 160px，不是 Pro 的非破坏视口；裁剪走“裁剪副本”进编辑器；复制不能选原图/当前视图。
5. Top 8 里只有 3 项可以完全不碰共享入口；其余都要在 `capture_view.rs` 加一处路由，建议把逻辑放进新模块，最后由一人合入路由。

## 差距表

图例：✅ 已实现　◐ 部分　✗ 未找到。“Pro”指截图Pro的目标行为；Pro 源码里也没有的会注明“Pro计划”。

### 选区

| 能力 | Pro 行为 | RustShot 现状 + 证据 | 差距 | 优先级 | 估计改动文件 |
|---|---|---|---|---|---|
| 1:1／锁比例 | Shift 框选 1:1；可锁当前比例或 4:3／16:9／自定义（Pro计划 P0-2，源码未找到） | ◐ Shift 新建选区为 1:1，Shift 拖角保持当前比例：`editor.rs:1652-1700`（`constrained_point`） | 没有固定比例预设，也不能持续锁定；只能按住 Shift | P1 | `editor.rs`（比例参数）、新 `capture_ratio.rs`、`capture_toolbar.rs` 更多菜单 |
| 输入精确宽高 | 点尺寸提示进入 X/Y/W/H 输入，Enter 应用、Esc 取消（Pro计划） | ✗ 尺寸标签只显示：`capture_canvas.rs:1282-1289` 渲染 `W × H · (x, y)`，没有点击或输入 | 整项缺失 | **P0** | 新 `capture_size_input.rs`；`capture_canvas.rs` 标签命中；`capture_view.rs` 状态与键路由 |
| 键盘像素精修 | 方向键移动；Shift+方向收缩、Ctrl+方向扩展，逐边调整 | ◐ 选区工具下方向键移动 1/10px，Alt+方向只改宽高（右、下边）：`capture_view.rs:6697-6740`；空编辑态移动：`capture_edit_arrow.rs:6`；框选前/拖动中 WASD 移光标：`capture_cursor_nudge.rs:16`；滚轮逐边 ±1px（功能说明 §3.2） | 键盘不能移动左、上边，也没有按边收缩/扩展；选区确定后 WASD 被工具键占用（合理，不改） | **P0** | `capture_edit_arrow.rs`（纯函数）、`capture_view.rs:6697` 一处分派、`tool_shortcut_hints.rs` |
| 全屏/当前屏 | Ctrl+A 当前屏，再按全部屏 | ✅ `capture_view.rs:1012`（`snap_screen_preview`）、`capture_range_shortcuts.rs:17-27`（Alt+A／Ctrl+Shift+A）；托盘“当前屏幕/所有屏幕截图” `tray.rs:127,133`；更多菜单 `capture_toolbar.rs:970-971` | 无 | — | — |
| 上次区域／最近选区 | 保存最近 10 个成功选区，上一个/下一个切换，拓扑变化时夹紧或跳过（S6、P1-4） | ✗ 未找到 `last_region`、`recent_region` 之类；`history.rs:349` 只记录图片，不记矩形 | 整项缺失 | **P0** | 新 `region_memory.rs`（独立 `regions.ini`）；`capture_view.rs` 在成功输出时记录并处理按键 |
| 截图界面内回放历史 | 截图界面里切到上一张/下一张历史画面，标明“历史+时间”，可回到实时屏幕（H1/H2） | ◐ 工具栏“历史”按钮只打开画廊窗口：`capture_view.rs:4171-4174`；`history.rs:43,117` 可读条目和原图 | 截图界面里不能翻历史 | P2 | `history.rs`、`capture_view.rs`、`canvas_source.rs`（换背景源） |
| 静态截图含光标 | 设置项 `capture.includeCursor`，加 `` ` `` 临时切换 | ✅ 偏好 `preferences.rs:21,51`；切换键 `capture_view.rs:6673` → `:4022`；只合成到输出 `capture_canvas.rs:759`、`capture_pin_output.rs:170` | 无 | — | — |
| 放大镜与取色 | 放大镜开关、倍数、网格；C 复制颜色并显示格式；Shift 切格式 | ◐ 开关在设置页 `settings_view.rs:710`；固定 15×11 格、每像素 12 DIP：`capture_canvas.rs:25-33`；C 复制 `capture_view.rs:6595-6605`；Shift 切 RGB/HEX、P 切坐标（功能说明 §3.1） | 倍数和网格不能调（Pro 清单也列为 P2） | P2 | `capture_canvas.rs`、设置页 |
| 辅助线 | 关／按 Alt／始终 | ✅ `preferences.rs:18` `guides` + Alt 临时显示 | 无 | — | — |

### 输出

| 能力 | Pro 行为 | RustShot 现状 + 证据 | 差距 | 优先级 | 估计改动文件 |
|---|---|---|---|---|---|
| 快速保存 | 不弹窗，按目录、模板、格式保存；保存成功有通知 | ◐ Ctrl+Shift+S `capture_view.rs:6609`，更多菜单 `capture_toolbar.rs:983-985`，流程 `capture_view.rs:4842`，规则 `save_options.rs:114`、`settings_save.rs`；成功后 `image_editor.rs:557` 直接关闭窗口 | 截图窗口关闭后没有成功通知，也没有“打开所在文件夹”；保存按钮不是分裂按钮 | P1 | `save_workflow.rs`、`ui.rs:463`（托盘气泡）、进程间通知 |
| 复制格式 | PNG + DIB；可选“复制为文件”（终端或 AI 粘贴） | ◐ `export.rs:453-481` 写 PNG + DIBV5 + DIB；图片没有 CF_HDROP，只有录屏复制文件 `recording_view.rs:3360` 和贴图拖出 `pin_drag.rs:22,51` | 缺“同时复制为文件” | P1 | `export.rs`（复用 `pin_drag::prepare` 的临时 PNG）、保存设置页 |
| 确认后的默认动作 | `after.confirmAction`：复制／贴图／快速保存／运行动作等（`CaptureAfterConfirmAction.cs:6-60`） | ✗ Enter／✓／双击固定为复制并结束（功能说明 §3.2；`capture_double_click.rs:377`）；未找到对应偏好 | 不能改成贴图或快速保存（Pro #345 P0） | **P0** | `preferences.rs`（或独立存储）、`settings_view.rs`、`capture_view.rs` 确认分派。OCR复制、表格等取值**交给快捷组合设计** |
| 贴图时同时复制 | `after.alsoCopyWhenPin` | ✗ 未找到 | 缺 | P2 | 同上 |
| 截图后交给动作／扩展 | 正式入口 O2 | — | **交给快捷组合设计** | — | — |

### 贴图

| 能力 | Pro 行为 | RustShot 现状 + 证据 | 差距 | 优先级 | 估计改动文件 |
|---|---|---|---|---|---|
| 透明度 | Ctrl+滚轮；菜单挡位 | ✅ `pin_view.rs:5132-5137` → `:3245`；菜单 100/75/50/25 `pin_menu.rs:639-657` | 默认透明度和步长不能设置（P1/P2 设置项） | P2 | `settings_pin.rs` |
| 鼠标穿透 | 菜单开启；全局入口可恢复；重启不恢复穿透 | ◐ Ctrl+T `host_shortcuts.rs:56` → `pin_view.rs:3227`；`pin_native.rs:916`；托盘“恢复贴图交互”（功能说明 §1） | 没有全局热键切换鼠标下贴图；Pro 有浮动恢复条 `PinClickThroughToolbarWindow` | P1 | `hotkey.rs`、`hotkey_registration.rs`、`tray.rs` |
| 标题 | F2 设标题，显示在贴图外侧，不进入导出 | ◐ 工作区可设标题 `pin_workspace.rs:62,108`、`pin_workspace_view.rs:931`；贴图窗口不显示，`pin_view.rs` 没有标题渲染 | 贴图上看不到，也不能在贴图上直接设 | P1 | `pin_view.rs`、`host_shortcuts.rs`（收起栏时 F2）、`pin_menu.rs` |
| 分组 | 新贴图进当前组；切组只改可见性 | ✅ `pin_workspace.rs:22-108`（最多 32 组，SwitchGroup／MovePin） | 无 | — | — |
| 锁定 | 禁止移动、缩放、编辑 | ✅ L `host_shortcuts.rs:69` → `pin_view.rs:3218` | Pro 的 `pin.lockMode` 子模式没有（P2） | — | — |
| 缩略图模式 | 右键框选非破坏视口，Shift 拖动平移，边框可调（B6） | ◐ Tab／双击只缩到 160px：`pin_view.rs:3362-3380` | 没有视口框选和平移 | P2 | `pin_view.rs`、`pin_geometry.rs` |
| 永久裁剪 | 缩略图状态下用菜单裁剪（B3） | ◐ “裁剪副本”打开编辑器裁剪副本：`pin_menu.rs:491`、`pin_view.rs:3381-3393` | 不能原地裁剪；有副本流程可用 | P2 | — |
| 关闭恢复／启动恢复 | 恢复最近关闭；启动时恢复贴图 | ✅ 托盘“恢复最近关闭” `tray.rs:162`；设置 `settings_view.rs:771`；启动恢复 `pin_workspace_view.rs:1709-1719` | 无 | — | — |
| 拖出／拖入 | Ctrl+左拖输出位图和 PNG | ✅ `pin_view.rs:4997`、`pin_drag.rs:22-108`、`pin_drop.rs` | 无 | — | — |
| 复制原图／当前视图 | `pin.copyContent` 或菜单二选一（Pin3） | ✗ 只有“复制图像”（当前合成） | 缺 | P2 | `pin_export.rs`、`pin_menu.rs` |

### 快捷键

| 能力 | Pro 行为 | RustShot 现状 + 证据 | 差距 | 优先级 | 估计改动文件 |
|---|---|---|---|---|---|
| 命令目录与帮助层 | `?` 打开分组帮助，提示和角标都来自命令目录（P0-1 第一步只读） | ◐ 托盘 MessageBox `tray.rs:255-270`，文本来自 `shortcut_help_context.rs:28`；角标/提示 `tool_shortcut_hints.rs`；截图内没有 `?`/F1（未找到） | 截图／贴图里不能直接查看完整快捷键 | **P0** | 新 `capture_help_overlay.rs`；复用 `shortcut_help_context.rs`；`capture_view.rs` 路由 |
| 应用内自定义 | 按作用域改键，Tooltip、角标、提示同步 | ✗ 会话内快捷键写死在 `capture_view.rs:6595-6680`、`host_shortcuts.rs:40-85`（功能说明 §7 第 12 条） | 整项缺失；需要先抽出命令表 | P1 | 新 `command_catalog.rs` + 各路由，工作量 L |
| 冲突检查 | 同作用域硬冲突不能保存 | ◐ 只有全局热键：录制冲突时保留原键 `settings_hotkey.rs:176`、`hotkey_registration.rs:90` | 会话内没有（因为不能自定义） | 跟随上一项 | — |
| 全局热键范围 | 截图、贴图、穿透、恢复等可绑定 | ◐ `HotkeyTarget` 只有 Capture／ClipboardPin（`hotkey.rs:11-15`） | 恢复贴图、穿透、当前屏截图没有全局热键；OCR 直达**交给快捷组合设计** | P1 | `hotkey.rs`、`hotkey_registration.rs`、`settings_hotkey.rs`、`tray.rs` |
| 编辑键语义 | Ctrl+Shift+Z 重做，Shift+Delete 清空 | ✅ `capture_view.rs:6645-6651`、`host_shortcuts.rs:51-53` | 无 | — | — |

### 其它 P0/P1

| 能力 | Pro 行为 | RustShot 现状 + 证据 | 差距 | 优先级 | 估计改动文件 |
|---|---|---|---|---|---|
| Esc 有标注时确认 | `capture.escConfirm` | ✗ Esc 直接取消整次截图并丢掉标注（功能说明 §7 第 10 条） | 容易误丢工作 | P2（建议只在有标注时确认） | `capture_view.rs` |
| 录屏 F9/F10 | P0-5 | ✅ `recording_shortcuts.rs`（功能说明 §5.3） | 无 | — | — |
| 选区圆角、阴影、边框（S7/O4） | P1 | ✅ 选区外观面板（功能说明 §2.1、开发计划“选区外观”） | 无 | — | — |
| 延迟截图 | C2（Pro 本轮不做） | ✗ 只有录屏延迟 | 低频 | P2 | `tray.rs`、`cli.rs` |

## Top 8 建议实现项

共享入口：`lib.rs`、`preferences.rs`、`capture_view.rs` 目前由其它会话在改（`capture_canvas.rs` 也有未提交改动）。约定：逻辑全部放在新模块，用纯函数加单测；共享文件只加 `mod` 声明和一处分派，由负责共享入口的人统一合入。新模块可以学 `capture_range_shortcuts.rs`，在 `capture_view.rs` 里用 `#[path]` 声明，就不用改 `lib.rs`。

1. **选区逐边键盘精修（P0）**：保留方向键移动、Shift 10px、Alt 改右/下边；新增 Ctrl+方向“对应边向外扩展”、Ctrl+Shift+方向“对应边向内收缩”，夹紧在画布内，最小 1px；按住重复沿用现有加速。
   允许改：`capture_edit_arrow.rs`（新增纯函数和测试）、`tool_shortcut_hints.rs`、`docs/behavior.md` §3.2。共享入口：**是**，只在 `capture_view.rs:6697` 分支前加一行调用。
   验证：`cargo test --lib edit_arrow`；真实界面未验时写明。
2. **选区 X/Y/W/H 输入（P0）**：点击尺寸标签或按 F2（选区工具且没有选中对象时）打开四个数字框，Enter 应用、Esc 取消，输入按物理像素并夹紧到虚拟桌面；只改选区，不动标注，也不写撤销历史。
   允许改：新 `capture_size_input.rs`（解析、夹紧和输入状态，可复用 `parameter_control.rs` 的数字输入样式）。共享入口：**是**，`capture_canvas.rs:1282` 标签命中 + `capture_view.rs` 键路由和状态。
   验证：`cargo test --lib size_input`（解析、负坐标、多屏夹紧、IME 优先）；深色主题下真实界面待验。
3. **上次区域／最近选区（P0）**：成功复制、保存、快速保存、贴图后记录选区矩形（最多 10 个，存在 `data_dir()/regions.ini`，和 `save_store.rs` 一样不碰 `settings.ini`）；框选前 `,`／`.` 切换上一个/下一个（Snipaste 同款，接入前要核对没有键冲突），显示器拓扑变化时夹紧或跳过。
   允许改：新 `region_memory.rs`。共享入口：**是**，`capture_view.rs` 的成功输出点和框选前键表各一处，再加 `lib.rs` 或 `#[path]` 声明。
   验证：`cargo test --lib region_memory`（去重、上限、拓扑变化、负坐标）。
4. **确认动作可配置（P0）**：设置 ▸ 截图里加“确认（Enter／✓／双击）”，可选复制并关闭（默认）／贴图／快速保存／保存面板。枚举和快捷组合设计共用一张表，OCR 复制等组合取值由那边加。
   允许改：`settings_view.rs`、新 `confirm_action.rs`（枚举、读写、分派表）。共享入口：**是**，`preferences.rs` 加一个字段（或独立存储以避开），`capture_view.rs` 和 `capture_double_click.rs` 的确认分派。**要先和快捷组合设计对齐再开工。**
   验证：`cargo test --lib confirm_action`、`cargo test --lib double_click`。
5. **截图内快捷键帮助层（P0，P0-1 第一步只读）**：截图和贴图里按 `?`（Shift+/）或 F1 打开半透明分组面板（当前可用、标注、输出、其它），内容复用 `shortcut_help_context::help_text` 拆出来的分组数据；Esc 关闭；跟随主题。
   允许改：`shortcut_help_context.rs`（拆出结构化分组，保留原文本 API）、新 `capture_help_overlay.rs`。共享入口：**是**，`capture_view.rs` 路由加一处渲染挂载；贴图侧改 `host_shortcuts.rs` 和 `pin_view.rs`（非共享）。
   验证：`cargo test --lib shortcut_help`；深色主题下不出现白底。
6. **快速保存反馈与“复制为文件”（P1）**：快速保存成功后由托盘进程弹气泡（`ui.rs:463 balloon`），显示路径，点击打开所在文件夹；保存设置里加“复制时附带 PNG 文件”（CF_HDROP，复用 `pin_drag::prepare` 的临时文件）。
   允许改：`export.rs`、`save_workflow.rs`、`settings_save.rs`、`save_store.rs`、`ui.rs`。共享入口：**否**（如果通知要走截图进程到托盘进程的消息通道，需要确认现有通道，可能涉及 `capture_view.rs`）。
   验证：`cargo test --lib export`、`cargo test --lib save_`。
7. **贴图标题显示与 F2（P1）**：工具栏收起时 F2 在贴图上编辑标题，写回 `pin_workspace` 的 SetTitle；标题显示在贴图外侧 chrome（悬浮时显示），不进入复制、保存和拖出；右键菜单加“设置标题”。
   允许改：`pin_view.rs`、`host_shortcuts.rs`、`pin_menu.rs`、`pin_workspace.rs`（只复用）。共享入口：**否**，但要避开贴图矢量文档第三阶段正在接入的段落。
   验证：`cargo test --lib host_shortcuts`、`cargo test --lib pin_workspace`、`cargo test --lib pin_export`（标题不进入导出）。
8. **全局热键扩展（P1）**：`HotkeyTarget` 增加“恢复最近关闭贴图”“切换鼠标下贴图穿透”“当前屏幕截图”，默认禁用，复用录制和冲突保护。OCR 直达热键**交给快捷组合设计**。
   允许改：`hotkey.rs`、`hotkey_registration.rs`、`settings_hotkey.rs`、`tray.rs`、`pin_native.rs`（按鼠标位置找贴图）。共享入口：**是**，`preferences.rs` 每个目标加两个字段（键和修饰键）。
   验证：`cargo test --lib hotkey`；`hotkey_registration_smoke` 只在用户同意时跑。

后排（不进 Top 8）：锁比例预设（P1）、会话内快捷键自定义与冲突检查（P1，等第 5 项的分组数据稳定后再做命令表）、截图界面内回放历史、缩略视口、复制原图/当前视图、Esc 有标注时确认、放大镜倍数和网格、延迟截图（均为 P2）。

完成任意一项后，按 AGENTS.md 更新 `docs/status.md` 一条，并在 `docs/roadmap.md` 功能对照的“窗口吸附/放大镜/取色/辅助线/指针”“保存/快速保存/复制”“贴图”“工具栏、主题、语言、常驻”对应行补差异。
