# 截图后快捷组合操作（快捷动作）设计

> 状态：首版已接主目录，包括设置与完成提示；实际验证与剩余范围见末尾。

## 0. 结论

1. **不做脚本引擎，做“快捷动作”**：一条动作由**采集源 + 是否先编辑 + 1–3 个固定步骤 + 历史策略**四个字段组成，全部是枚举，可打包成一个 `u32`。一个动作可以绑定全局热键、放进托盘子菜单，也可以用命令行 `rustshot --action <id>` 触发。
2. **默认截图热键本身就是一条快捷动作**：`{框选, 先编辑, [复制图片]}`。“Enter／双击／✓ 完成时做什么”就是这条默认动作的步骤，所以“完成动作可配置”不需要另做一套机制。
3. **不先编辑的动作在选区完成时立即执行**（拖完松手、单击采用窗口，或固定源冻结后马上执行）。**失败不关窗**：回到正常编辑栏，选区保留，状态栏说明原因，而不是弹窗后无法回退。
4. **启动路径零新增 I/O**：动作编码只随现有 IPC 走（待命元数据里空着的 `reserved` 字段和 `CAPTURE_MESSAGE` 的 WPARAM 高位），截图进程冻结前不读任何配置文件。“上次区域”只在冻结之后才读一个 32 字节的小文件。
5. **历史一次会话最多写一次**：现在复制、保存、贴图三条路径各自写历史，组合“复制+贴图”时会写两次，必须先加会话级去重。只出文字的动作（OCR、表格、扫码）默认不写历史。

## 1. 设计约束

- **立即执行**：不先编辑的动作在选区完成时就执行；再要求一次确认，快捷动作就失去意义。需要调整的人用默认的先编辑动作。
- **完成动作只作用于直接截图**：带自身步骤的快捷动作不继承全局“完成时做什么”，免得个人习惯改变自动化的输出。
- **历史只写一次**：全局开关允许**并且**本次动作也允许才写历史；只返回区域、录屏、长截图不入库；同一会话不因多个出口重复。
- **失败可回退**：识别失败或结果为空时回到编辑态并说明原因，不弹阻断式对话框。
- **数据形状**：只借用“副作用 + 会话去向 + 通知”这一形状，不引入脚本运行时。

## 2. RustShot 现状（文件:行号）

**入口与参数传递**

- 热键：`hotkey.rs:11` 的 `HotkeyTarget` 只有 `Capture` 和 `ClipboardPin` 两个目标。`hotkey_registration.rs:103-106` 已把 ID 区间 `1..0x5fff` 和 `0x6000..0xbfff` 全部占用，新增目标必须重新划分。设置界面在 `settings_hotkey.rs:238-248`，偏好在 `preferences.rs:23-26, 314-337`（`hotkey`、`custom_hotkey`、`pin_hotkey`）。
- 常驻进程（`ui.rs`）：`WM_HOTKEY | CAPTURE_MESSAGE` 在 `:1091`，剪贴板贴图热键在 `:1078`。`worker_argument`（`:170`）把 WPARAM 的低 4 位映射成 `--region/--fullscreen/--all-screens/--history/--settings`。托盘分发在 `:1228-1260`。第二次启动 `rabshot.exe` 时，`:640-668` 把 `CAPTURE_MESSAGE(WPARAM 0, QPC)` 投给已有实例，这是 CLI 转发的现成通道。
- 托盘：`tray.rs:17-32` 定义菜单 ID，`:102` 的 `menu()` 每次弹出都重建，并已读取 `Preferences`。
- CLI：`cli.rs:518, 533-547` 的 `rabshot-cli --capture` 直接拉起 `rabshot-gpui --region`，不经过常驻进程和待命进程。`file_launch_command.rs` 只负责文件贴图和编辑。
- 待命进程：`capture_standby.rs:55-71` 的 `RequestMetadata` 有 `mode` 和**未用的 `reserved: AtomicU32`**，布局用断言锁定为 80 字节。`publish_request`（`:153`）和消费端（`:1891-1947`）都校验 `mode<=2`，得到 `CaptureRequest{full_screen, all_screens}`（`:1479`）。随后经 `lib.rs:307/319` 进入 `gpui_capture::run`（`:125`）。固定源的预设裁剪在 `gpui_capture.rs:477` 和 `:834` 计算。
- 原生首屏交接：`startup_handoff.rs:107` 把首屏里已完成的选区交给 GPUI。早期输出种类见 `startup_output.rs:9`（Pin/Confirm/ControlText/Ocr），在 `startup_handoff.rs:561` 重放成 `Action::Pin/Copy`。

**完成与输出**

- 完成语义目前硬编码为复制：Enter 在 `capture_view.rs:6577`，双击在 `:3212`（决策见 `capture_double_click.rs:320-346`），✓ 在 `capture_toolbar.rs:886`（提示“复制并完成”），原生首屏双击或 Enter 在 `startup_handoff.rs:562`。
- 动作分发：`capture_view.rs:4101-4232`。Copy 走 `:4728/4745`（`copy_image_result`，成功后写历史并调用 `image_editor.rs:557 finish_output` 关窗）。Pin 走 `:4110` 到 `capture_pin_output.rs:117`。QuickSave 走 `:4842`，到 `save_workflow::quick_save`，历史在 `save_workflow.rs:78`。Qr 在 `:4176`（结果显示在面板）。Ocr 在 `:4211`。TranslateImage 走 `capture_image_translate.rs:83`（百度引擎会打开图片编辑窗）。
- **已有“直接动作”**：框选前 Enter 复制固定预览或整屏，中键贴图，Ctrl+Shift+S 快速保存。另有“识别文字后直接复制”：偏好 `text_capture_copy_only`（`preferences.rs:20/106`，设置项 `settings_view.rs:739-742`），只对 R／Shift+C 生效。链路是 `recognize_with_copy_policy`（`capture_view.rs:4338`，`copy_only` 在 `:4430`）→ `Completion::CopyAndClose`。剪贴板失败时由 `capture_text_result.rs:46-61` 的 `publish_auto_copy` 和 `retain_auto_copy_failure`（`capture_view.rs:4923`）保留结果。**OCR→复制所需的失败回退已经具备，只差一个不依赖偏好的触发开关。**
- 选区完成的挂点：`capture_view.rs:3670`（`toolbar_visibility.finished_new_crop`，拖动或单击采用后），以及 `:3616`、`capture_range_shortcuts.rs:275`、`startup_handoff.rs:107`。二维码自动扫描在选区稳定 140ms 后进行（`capture_qr.rs:15`）。
- 历史写入点：`history.rs:349/355`（读取 `Preferences.history`，默认关闭）、`capture_view.rs:4748`、`save_workflow.rs:78`、`pin_view.rs:481`、`long_editor.rs:655`。**没有会话级去重。**
- 通知：只有常驻进程的托盘气泡 `ui.rs:463 balloon`，没有轻量提示，截图进程退出后也无法再显示任何东西。
- 表格：`recognition_table.rs` 和 `ocr_table_copy_ui.rs` 已能对可信矩阵生成 TSV（贴图菜单“复制表格”）。表格识别升级另有调研。

## 3. 数据模型（新模块 `quick_action.rs`）

```rust
enum Source { Region, LastRegion, CurrentScreen, AllScreens, ActiveWindow }
enum Step {               // 4 bit，0 = 空
    CopyImage, Pin, QuickSave,               // 图像输出
    OcrCopy, TableCopy, QrCopy,              // 文本输出（写剪贴板）
    QrOpen, TranslatePin,                    // 终结步骤
}
enum HistoryPolicy { Inherit, Never }
struct QuickRecipe { source: Source, edit: bool, steps: [Option<Step>; 3], history: HistoryPolicy }
struct QuickAction { id: String, name: String, recipe: QuickRecipe,
                     hotkey: Option<HotkeySpec>, in_tray: bool, enabled: bool }
```

- **校验**（纯函数，带测试）：至少 1 步、最多 3 步。最多一个写剪贴板的步骤（CopyImage、OcrCopy、TableCopy、QrCopy 互斥）。`Pin`、`TranslatePin`、`QrOpen` 只能放在最后。`QuickSave` 可以和任何步骤组合。
- **执行语义**：步骤按顺序执行，前一步成功才执行下一步。全部成功后关窗并发送提示。任何一步失败就停在编辑态，已经完成的副作用保留（例如图片已存，复制失败），状态栏列出“已完成 X，Y 失败：原因”。
- **打包成 `u32`**（`QuickRecipe::pack/unpack`）：bit 0-2 采集源，bit 3 先编辑，bit 4-15 三个步骤，bit 16 历史策略，bit 24-31 魔数 `0xA5`。值为 0 表示没有快捷动作，即旧行为。
- **为什么不做通用脚本**：一是启动路径必须零 I/O、零解释器；二是现有步骤都要访问 GPUI 会话状态（OCR 层、剪贴板失败回退、贴图交接），脚本化需要先抽一套宿主 API，设计量很大；三是用户要的是“一键完成常见组合”，枚举加校验就能覆盖，设置页也能用下拉框完成。
- **将来扩展**：新增 `Step` 变体，例如 `Record`、`LongCapture`、`RunCommand{exe,args}`（把临时 PNG 路径交给外部程序，即命令型扩展）、`FormulaCopy`。4 bit 还有 7 个空位。需要参数的步骤，把参数放在动作表里，用 `u32` 里的槽位号引用。真要脚本时，在 `Step::Script(id)` 下接 PostPlan 形状的结果，不改现有枚举。

**内置预设**（id 也是 CLI 名，全部默认**不绑定**热键）：

| id | 名称（中 / 英） | 配方 |
|---|---|---|
| `capture` | 截图 / Capture | 框选，先编辑，[复制图片]（即现有默认热键，步骤可改） |
| `copy` | 框选即复制 / Select & copy | 框选 → 复制图片 |
| `pin` | 框选即贴图 / Select & pin | 框选 → 贴图 |
| `copy-pin` | 复制并贴图 / Copy & pin | 框选 → 复制图片 → 贴图 |
| `quick-save` | 框选即保存 / Select & save | 框选 → 快速保存 |
| `ocr-copy` | 识别并复制文字 / OCR → copy | 框选 → OCR 复制 |
| `table-copy` | 识别表格并复制 / Table → copy | 框选 → 表格复制（接口预留） |
| `translate-pin` | 图片翻译贴图 / Translate → pin | 框选 → 翻译后贴图 |
| `qr-copy` / `qr-open` | 扫码复制 / 扫码打开 | 框选 → 扫码复制 / 打开链接 |
| `last-copy` | 上次区域复制 / Repeat last → copy | 上次区域 → 复制图片 |
| `screen-save` | 当前屏保存 / Screen → save | 当前屏幕 → 快速保存 |
| `window-save` | 当前窗口保存 / Window → save | 当前窗口 → 快速保存 |

**存储**：新文件 `%LOCALAPPDATA%/RustShot/quick_actions.ini`，由 `quick_action_store.rs` 负责，**不动 `preferences.rs`**。每个动作一节 `[action.<id>]`，记 name/recipe/hotkey/tray/enabled。内置项只保存差异，比如用户改过的步骤或绑定的热键。`capture` 节的步骤就是“完成动作”。

## 4. 交互

- **触发时机**：先编辑的动作走正常编辑，Enter／双击／✓ 执行它的步骤。改法是新增 `Action::Confirm`，按会话配方解析，替换上述 4 处硬编码的 `Action::Copy`；Ctrl+C 仍固定为复制图片。不先编辑且采集源是框选时，在 `finished_new_crop`（`capture_view.rs:3670`）和首屏交接完成（`startup_handoff.rs:107`）后执行；框选前 Enter 采用固定预览也算选区完成。固定采集源在 `screenshot_output_ready()` 第一次为真时执行。
- **选区阶段提示**：GPUI 就绪后，在现有左下角快捷键提示区显示 `「识别并复制文字」· Esc 取消`（`trf!("{} · Esc 取消","{} · Esc to cancel")`）。v1 不在原生首屏（`startup_surface.rs`）加文字，以免碰这个 1 万行的启动关键文件。十字光标旁的小标签作为第二阶段。
- **执行中**：OCR 和表格复用现有扫描光带和“识别中”提示；翻译复用进度状态。此时 Esc 取消正在执行的步骤并回到编辑栏（与图片翻译现有的“Esc 退出即取消”一致），再按一次 Esc 才退出截图。
- **失败回退**（一律留在会话，显示工具栏，保留选区）：

| 情况 | 处理 |
|---|---|
| OCR 无文字 | 状态“未识别到文字”，不进入文字层 |
| OCR 模型未就绪 | 走现有首次下载进度；下载失败按 OCR 失败处理 |
| 表格证据不足或接口未就绪 | 降级为显示原位文字层，状态“未找到可靠表格，已显示文字” |
| 翻译未配置或网络失败 | 状态显示原因，标注不变（现有逻辑） |
| 剪贴板被占用 | 现有 `retain_auto_copy_failure`，结果保留可重试 |
| 无二维码 / 无链接 | 状态“未发现二维码 / 内容不是链接”，`QrOpen` 不隐式复制 |
| 上次区域失效（显示器拓扑变化、超出桌面） | 退回普通框选，提示“上次区域不可用，请重新框选” |
| 快速保存失败 | 现有逻辑（不关窗） |

- **完成提示**：截图进程关窗前用 `WM_COPYDATA` 把一行文字发给常驻窗口 `RustShot.MVP.Window`。常驻进程新增 `quick_toast.rs`：一个用 GDI 绘制、半透明、不抢焦点、点击穿透的小窗，跟随主题，显示在选区所在屏的右下角，约 1.5 秒后消失。文案示例：`已复制 128 字 / Copied 128 chars`、`已复制表格 5×3 / Copied 5×3 table`、`已保存：文件名 / Saved: name`。没有常驻进程（`rabshot-cli` 直接启动）时不显示提示。只复制图片和贴图默认不提示，因为结果本身可见。
- **设置页**（设置 ▸ 快捷键，在现有两条热键下方新增“快捷动作 / Quick actions”分区，模块 `settings_quick_actions.rs`）：
  - 列表行：启用开关｜名称｜配方摘要“框选 → OCR → 复制”｜热键胶囊（点击录制，Backspace 禁用，沿用 `settings_hotkey` 录制流程）｜托盘复选框｜编辑／删除（内置项只有“恢复默认”）。
  - 编辑区：名称；采集（`框选/上次区域/当前屏幕/所有屏幕/当前窗口` · `Region/Last region/Screen/All screens/Window`）；`先编辑再执行 / Edit first`；步骤 1–3 下拉（`复制图片/贴图/快速保存/识别并复制文字/识别表格并复制/扫码复制/扫码打开/翻译后贴图` · `Copy image/Pin/Quick save/OCR → copy/Table → copy/QR → copy/QR → open/Translate → pin`）；`写入历史 / History`：`跟随设置 / Follow setting`、`不写入 / Never`；底部一行显示校验错误；`应用 / Apply`、`取消修改 / Revert`。
  - 设置 ▸ 截图 新增一行“完成动作（Enter／双击／✓） / On finish”，下拉选 `复制/贴图/快速保存/复制并贴图`，写入的就是 `capture` 动作的步骤。
  - 热键冲突检查范围扩大到截图、剪贴板贴图和全部快捷动作。
- **托盘**：在“区域截图”下方加 `快捷动作 ▸`，列出 `in_tray && enabled` 的动作，右侧显示热键。菜单 ID 从 `0x100 + 序号` 起。
- **CLI**：`rabshot.exe --action ocr-copy`（或 `--action <自定义 id>`）。已有实例时沿用 `ui.rs:640-668` 的转发，把编码后的配方放进 WPARAM 高位投给常驻进程，走待命快路径；没有实例时启动常驻进程后再执行。`rabshot-cli --capture --action ocr-copy` 直接拉起 `rabshot-gpui --region --quick 0xA5…`，供脚本或调试使用。未知 id 返回错误，不退回普通截图。

## 5. 性能（约 200ms 启动口径）

- 常驻进程：`quick_actions.ini` 只在启动、收到 RELOAD 和弹出托盘菜单时解析。热键事件只按注册 ID 查表得到 `u32`，没有 I/O。
- 待命进程：配方写进 `RequestMetadata.reserved`（布局不变，仍 80 字节）。`mode` 仍为 0..2，“上次区域”和“当前窗口”冻结整桌面时都用 mode 0，校验逻辑不变。非待命路径用 `--quick <hex>` 参数。
- 截图进程：冻结前不读文件。“上次区域”在冻结完成、计算裁剪（`gpui_capture.rs:477/834`）时读取 `last_region.ini`（新模块 `last_region.rs`，每次输出成功后写入全局物理像素矩形）。“当前窗口”在消费请求时调用 `GetForegroundWindow` 并读 DWM 扩展边框；截图层尚未前台化，所以拿到的仍是用户窗口。
- 改到启动路径的切片（S3）完成后，用现有 `RUSTSHOT_STARTUP_REQUEST_QPC` 启动埋点，对比带配方和不带配方的“请求→可框选”耗时各测一次。不跑 `benchmark.ps1`。
- 固定采集源不先编辑时，v1 仍走会话路径：先冻结，再按预设裁剪执行，然后关窗。可能出现一帧遮罩，需用 `flicker-probe` 验证。若闪烁明显，第二阶段为“固定源 + 只做复制或保存”增加不建窗口的直出路径，复用 `fast_capture` 的冻结结果。

## 6. 实现切片

共享入口（`lib.rs` 的 `mod` 行、`capture_view.rs` 的挂点与 `Action::Confirm`、`preferences.rs`、`Cargo.toml`）**由主会话统一改**。各切片只交付新模块和需要主会话插入的挂点清单。`capture_view.rs`、`preferences.rs`、`lib.rs` 目前正被其它会话修改。

| # | 内容 | 允许改／新建 | 交付与验证 |
|---|---|---|---|
| S1 模型 | `Source/Step/QuickRecipe` 打包与解包、校验、内置预设、CLI 名解析、`tr!` 标签；INI 存储（解析、序列化、内置差异、损坏行跳过） | 新建 `src/quick_action.rs`、`src/quick_action_store.rs`、`src/quick_action_tests.rs` | `cargo test --lib quick_action`（往返、非法组合、魔数、旧文件、热键冲突集合）。无依赖，可最先并行 |
| S2 会话执行 | 会话状态机（待执行 → 第 i 步 → 完成或回退）、`Action::Confirm` 解析、各步骤调用现有函数、历史去重、选区阶段提示文案；纯决策部分放进 `quick_action_flow.rs` | 新建 `src/capture_quick_action.rs`（`impl CaptureView`，仿 `capture_image_translate.rs`）、`src/quick_action_flow.rs`；**挂点由主会话插入** `capture_view.rs:3670/6577/3212/4101`、`capture_toolbar.rs:886`、`startup_handoff.rs:107/562` | `cargo test --lib quick_action_flow`（触发时机、失败回退、Esc、历史只写一次、TableCopy 未就绪时降级）。真实界面待验 |
| S3 传输 | WPARAM 高位与 `reserved` 编码、`--quick` 参数、`CaptureRequest.quick`、`gpui_capture::run` 增参、上次区域读写、当前窗口矩形 | `src/capture_standby.rs`、`src/ui.rs`（仅 `worker_argument` 和 dispatch）、`src/gpui_capture.rs`、`src/bin/rustshot-gpui.rs`、新建 `src/last_region.rs`；`lib.rs:307/319` 的签名改动交主会话 | `cargo test --lib capture_standby`、`cargo test --lib last_region`。启动埋点对比一次 |
| S4 热键、托盘、CLI | ID 区间重划（Pin 改为 `0x6000..0x7fff`，快捷动作用 `0x8000..0xbfff`，分 16 槽，每槽 `0x400`）、多条注册与失败回退、托盘子菜单、`--action` 转发 | `src/hotkey_registration.rs`、`src/ui.rs`（`reload_hotkey` 和 `run_impl`）、`src/tray.rs`、`src/cli.rs` | `cargo test --lib hotkey_registration`、`cargo test --lib quick_action`（CLI 解析）。依赖 S1 和 S3 |
| S5 设置页 | 快捷动作分区、编辑区、“完成动作”下拉、冲突检查 | 新建 `src/settings_quick_actions.rs`；`settings_view.rs` 只接一行入口（交主会话）；`settings_hotkey.rs` 冲突函数 | 纯校验复用 S1 测试；窗口必须跟随深色主题，真实界面待验。依赖 S1 |
| S6 完成提示 | 常驻进程 GDI 提示窗、`WM_COPYDATA` 协议（带版本号，长度不超过 256 字符） | 新建 `src/quick_toast.rs`；`ui.rs` 的 dispatch 只加一个分支 | `cargo test --lib quick_toast`（消息编解码、截断）。观感用 `flicker-probe` 自测 |

**表格接口预留**：S2 定义 `trait TableSource { fn table_tsv(&RecognizedText) -> Option<TableCopy{tsv, rows, cols}> }`。v1 实现直接调用 `recognition_table.rs` 现有的可信矩阵逻辑（与贴图“复制表格”相同）。返回 `None` 时按 §4 降级。表格调研定稿后只替换这个实现。

建议顺序：S1 → S3、S2 并行 → S4、S6 → S5。最小可用版本是 S1、S2、S3、S4，已能实现热键直接“框选 → OCR → 复制”。

## 7. 首版默认与实现

1. 失败保留选区和已完成步骤，Enter 再执行时从失败步骤继续；Esc 先取消运行中的步骤。保存在线程中编码，写文件前检查取消；已写入的文件保留，回执不会让重试重复保存。打开链接进入系统提交后等待回执，Esc 不丢弃已开始的打开结果，避免重试再次打开。
2. OCR／表格／扫码预设默认 `Never`，图像预设 `Inherit`；图像、保存与贴图共用会话历史去重。历史失败作为已完成输出的提示，不重做剪贴板或文件副作用。
3. ✓、Enter 和双击使用 `Confirm`；普通截图的完成配方在冻结后加载，CLI、全屏和全部屏幕入口都沿用。截图态 Ctrl+C 仍直接复制图片，结果文字层保留原复制交互。
4. 13 个内置预设默认不绑定额外热键；最多 16 个动作热键，注册失败只回退相应绑定，Ctrl／Alt 位与 Win32 互换已有往返测试。

实现分布：`quick_action`／`quick_action_store` 管理模型和差异配置，`quick_action_flow` 管理顺序与重试；`capture_quick_action` 调用现有输出，`settings_quick_actions` 编辑配方；`quick_toast` 经有界 `WM_COPYDATA` 消息由常驻进程绘制提示，长路径按字符边界缩为 256 字符以内。待命元数据仍为 80 字节，IPC 版本升级并双端校验配方。

上次区域保存为带校验的 32 字节文件，冻结后才读；负屏幕坐标按全局物理坐标存储，桌面边界变化时回到普通框选。当前窗口在创建截图层之前取扩展边框；托盘菜单可能影响前台窗口身份，实际应用场景仍待验证。

验证：三个程序编译检查／调试构建和 `cargo test --lib quick_` 20 项通过，待命通信 11 项、上次区域 2 项、热键注册专项通过，新版已更新托盘。未知动作的实际 CLI 入口返回错误且不捕获。三轮双屏拖动遮罩探针无无罩帧。

本机各测一次真实托盘通知到冻结可框选的原生 Ready 埋点：普通入口 129.6ms，带框选编辑配方 99.4ms，均取得前台并正常取消；这是单次对比，不代表耗时分布。设置实际交互、完成提示观感及当前窗口托盘入口仍待自然使用确认。
