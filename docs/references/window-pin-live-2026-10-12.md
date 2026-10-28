# 实时窗口贴图（窗口投影 + 可选输入转发）调研

> 状态：第一档已进入主目录实现；实际验证与限制见末尾。

## 结论

1. **第一档用 DWM 缩略图（`DwmRegisterThumbnail`）**：系统合成直接把源窗口画进贴图窗，我们进程没有逐帧拷贝、读回或 GPUI 上传；被遮挡的窗口照样实时显示（Chromium、Electron、WPF 都可以，因为 DWM 本来就有它们的表面）；源区域、缩放、透明度都是 `DWM_THUMBNAIL_PROPERTIES` 里的字段。API 用 `pin_native.rs` 现有的手写 `extern "system"` 方式声明，**不改 Cargo.toml**。
2. **像素只在需要时取一次**：转为静态贴图、OCR、复制、拖出时才抓一帧。首选 Windows.Graphics.Capture（WGC）单帧（遮挡时也能抓，Win11 可去掉黄框）；其次 `PrintWindow(PW_RENDERFULLCONTENT)`；最后回退到截图时已冻结的选区像素。实时帧不进 GPUI，符合架构约定 6“没有外部纹理导入接口”。
3. **不采用** PrintWindow 逐帧方案：遮挡的 Chrome 经常抓到全黑，只能临时 TOPMOST 再恢复 Z 序，会改动别的程序窗口的叠放顺序，RustShot 不能接受。也**不采用** DXGI 桌面复制裁剪：被遮挡就抓到遮挡物，每帧整屏 Acquire，而且和“禁止逐帧读回”的精神冲突。
4. **输入转发分档、默认关闭**：第二档点击／滚轮转发，Win32 消息投递优先，失败时走 UIA 模式（Invoke／Scroll）兜底；第三档键盘只给经典编辑控件做实验，或者干脆改成“前置源窗口”。滚轮更适合用来缩放贴图，转发收益低、风险高。
5. **主要未知点**要先用一个小切片验证：缩略图画在 GPUI（DirectComposition）窗口里时，和窗口自身内容的上下关系；和 `WS_EX_LAYERED` 透明度能否共存；DPI 虚拟化源窗口的 `rcSource` 坐标；源窗口最小化时缩略图的表现。

## 1. 平台已知问题

- PrintWindow 对普通 Win32／WPF 稳定；**被遮挡的 Chrome／GPU 窗口经常全黑**。临时置顶再抓会改动其它程序的 Z 序，仍不可靠；长久方案是 Graphics Capture。
- 判断是否被遮挡时，要排除各类“软覆盖层”：鼠标穿透窗口、`WDA_EXCLUDEFROMCAPTURE`、cloaked 窗口、layered 加 tool／noactivate 的窗口、alpha<96 的 layered 窗口。否则录屏边框和贴图自身会被当成遮挡物，导致遮罩闪烁。
- `PrintWindow` 会被挂死的源窗口阻塞：停止采集时不在 UI 线程 join，缓冲区由采集线程统一回收。
- 最小化（`IsIconic`）：冻结最后一帧、降频轮询，显示“恢复／前置／关闭”，不自动隐藏贴图。源窗口关闭：停止刷新并提示。
- 输入转发：源窗口被遮挡时 `WindowFromPoint` 会命中上层窗口，必须在源窗口树内命中，并用 `PostMessage`／`SendMessageTimeout(ABORTIFHUNG)`。Chrome 忽略 lParam 而读 `GetCursorPos`，属于平台边界。`SetCursorPos`+`SendInput` 会让光标跳动，不采用。完整性级别更高的源窗口要禁用转发（UIPI）。
- **DPI 感知**：RustShot 是 PMv2，读到的是物理坐标；投递给 DPI 不感知或系统感知窗口的 `WM_*BUTTON*` 坐标要用对方的逻辑坐标（`PhysicalToLogicalPointForPerMonitorDPI` 或按 `GetDpiForWindow` 换算）。`WM_MOUSEWHEEL` 的 lParam 是屏幕坐标，同样受影响。

## 2. RustShot 现状（与本功能相关）

- 贴图：`capture_pin_output.rs::pin_range_started` 在截图 GPU 进程内调用 `PinView::open_capture_handoff`，截图窗口关闭后，贴图仍留在同一个 `rabshot-gpui` 进程里。`pin_native.rs` 已有 `place`／`topmost`／`opacity`（`WS_EX_LAYERED`+`LWA_ALPHA`）／`click_through`，以及手写的 dwmapi extern（`DwmFlush`、`DwmGetWindowAttribute`）。GPUI 窗口用 DirectComposition 呈现。
- 实时采集：只有 DXGI 桌面复制（`live_capture.rs`、`capture_frame_dxgi.rs` 的帧租约保证每次 Acquire 都 Release；`recording_gpu.rs` 用独立视频设备）。**没有 WGC、PrintWindow、DWM 缩略图**；`windows` 0.61.3 的 WinRT 只开了 `Foundation`／`Graphics_Imaging`／`Media_Ocr`／`Storage_Streams`。
- 窗口身份：`selection_assist::WindowTarget` 在冻结时记录 `hwnd`／`pid`／`tid`／`global_bounds`，`control_selection.rs` 已能从指定 HWND 子树做 UIA 命中（后台 MTA、有预算、校验进程身份），可以复用到第二档的 UIA 兜底。
- 已经在用 `WDA_EXCLUDEFROMCAPTURE` 的窗口有长截图边框和预览窗。进程为 PMv2（`cli.rs`、`capture_standby.rs`）。

## 3. 采集方案对比

| 方案 | 能否接 GPUI | 我方开销 | 帧率 | 遮挡 | 最小化 | 主要问题 |
|---|---|---|---|---|---|---|
| **DWM 缩略图** | 不经过 GPUI：DWM 直接合成进目标顶层 HWND 的指定矩形 | ≈0（无拷贝，DWM 负责缩放） | 跟随源窗口和合成器 | **正常**（含 GPU／Chromium） | 一般显示最后内容，需实测 | 拿不到像素；目标必须是顶层窗口；GPUI 不能在缩略图上面叠画；受保护内容（DRM、WDA）显示为黑 |
| **WGC 按 HWND** | 只能 Map→CPU→`RenderImage`→atlas 上传，并 `drop_image` 旧帧 | 每帧 GPU→CPU 加 CPU→GPU 各一次；1000×700@30fps 估算约 84MB/s 双向，外加整窗重绘 | 跟随源窗口，最高到刷新率 | **正常** | 不出新帧 | 要增加 `Graphics_Capture` 等 Cargo feature；Win10 有黄框（Win11 可用 `IsBorderRequired=false`，需先 `RequestAccessAsync(Borderless)`，未打包应用一般直接允许，需实测）；抓的是整窗含非客户区，要自己算客户区偏移；窗口尺寸变化要 `Recreate` 帧池 |
| **PrintWindow(PW_RENDERFULLCONTENT)** | 同上，CPU 位图→`RenderImage` | 每帧整窗 GDI 位图（4K 窗口约 33MB）再裁剪，同步调用 | 通常 10–30fps，大窗更低 | Win32／WPF 正常；**Chrome／GPU 窗口常全黑** | 失败 | 挂死的源窗口会阻塞调用；TOPMOST 补救会改动别人的 Z 序 |
| **DXGI 复制裁剪** | 同上 | 每帧对整个输出 Acquire→拷贝→读回 | 跟随桌面 | **抓到的是遮挡物** | 抓不到 | 本质是“屏幕区域直播”而不是窗口贴图；贴图自身必须 `WDA_EXCLUDEFROMCAPTURE`，否则出现画中画；和冻结预览禁止逐帧读回的原则冲突 |

**DWM 缩略图作为“只看不点”第一档的评估**：可行，而且是唯一完全不逐帧拷贝的方案。

- 区域裁剪：`DWM_TNP_RECTSOURCE`，可配合 `fSourceClientAreaOnly`。`rcSource` 用哪个原点（整窗还是客户区）、在 Win10/11 不可见缩放边框下怎么偏移，必须实测；建议用 `DWMWA_EXTENDED_FRAME_BOUNDS`、`GetWindowRect`、`ClientToScreen` 三者推算，再与 `DwmQueryThumbnailSourceSize` 交叉检查。
- 缩放：`rcDestination` 决定显示尺寸，沿用现有贴图缩放预算。透明度：`DWM_TNP_OPACITY`（0–255），不依赖 `WS_EX_LAYERED`。置顶：宿主窗口 `topmost`。显示／隐藏：`DWM_TNP_VISIBLE`。
- 源窗口移动或最大化：相对客户区矩形保持不变，但窗口原点到客户区的偏移会变，要重算 `rcSource` 后调用 `DwmUpdateThumbnailProperties`。建议在 GPUI 定时器里以 4–10Hz 轮询 `GetWindowRect`／`ClientToScreen`／`IsIconic`／`DWMWA_CLOAKED`／`IsWindow`，这些都是廉价调用；不加 WinEvent 钩子，符合“WindowProc 只转发”的约束。
- 叠放关系：缩略图一般画在目标窗口自身内容**之上**。因此提示、角标等 GPUI 内容只能画在内容矩形以外（边框、阴影、独立工具栏窗口），或者先隐藏缩略图，再用 GPUI 画静态帧和遮罩。DComp 窗口上的实际叠放、与 `WS_EX_LAYERED` 是否兼容，是切片 2 的首要实验；如果不行，就改用一个不带 GPUI 的原生宿主顶层窗口（由 GPUI 贴图窗口拥有），只承载缩略图。
- 源窗口关闭后缩略图立即变空，事后无法补抓。回退帧用**截图时冻结的选区像素**（`CaptureDocumentPayload` 里已经有），可以作为首帧占位和关闭后的显示。

## 4. 分阶段方案

| 档 | 内容 | 主要风险 | 验收 |
|---|---|---|---|
| **一档 只读实时窗口贴图** | 截图选区→“窗口贴图”，锁定源 HWND 和客户区相对矩形，用 DWM 缩略图显示；支持拖动、滚轮缩放、`Ctrl+滚轮`透明度、中键恢复 100%、置顶、鼠标穿透、关闭；状态区分 正常／最小化／cloaked（其它虚拟桌面）／已关闭／不可用；右键菜单含“转为静态贴图”“前置源窗口”“恢复源窗口” | DComp 叠放和 layered 兼容；`rcSource` 偏移；DPI 虚拟化源窗口；最小化表现 | 记事本、资源管理器、WPF、Chrome／Edge（被遮挡）、VS Code、视频播放器均实时刷新；移动、最大化、跨屏、跨 DPI 后区域不漂移（目视误差 ≤2px）；源最小化或关闭有明确结局；我方进程 CPU 接近 0 |
| **二档 点击／滚轮转发** | 显式“交互”开关（工具栏按钮，或按住修饰键），打开后内容区的左键和滚轮转给源窗口，拖动贴图改为拖边框或 `Alt+拖`；不开时滚轮仍是缩放 | 浏览器、DirectManipulation、UWP 不接受投递消息；源窗口在后台时焦点规则不确定；UIPI；DPI 坐标换算 | 经典控件（按钮、列表、编辑框、树）在源窗口被遮挡时也能点能滚；Chrome 至少“部分可用或明确失败”并记入样本表；永不移动真实光标；穿透和交互互斥 |
| **三档 键盘** | 实验项。选项 A：把 `WM_KEYDOWN`／`WM_CHAR` 投递给 `GetGUIThreadInfo(源线程).hwndFocus`（只适用于经典 EDIT／RichEdit，不支持 IME）。选项 B：“前置源窗口”交接，键盘直接进源窗口，但源窗口会升到前面 | 快捷键和加速键语义错乱、IME、安全软件告警 | 只要求经典编辑框能输入 ASCII；其它应用明确标记“不支持”。建议拍板后再做 |

**与静态贴图的关系**：实时贴图是运行时会话。“转为静态”时抓一帧，经现有图像贴图路径（与剪贴板／图片贴图同一入口）在原位置打开普通 `PinView`，然后关闭实时贴图；OCR、复制、拖出、标注都由静态贴图承担，实时态不做自动 OCR。复制或保存可以在实时态直接触发，等同“抓一帧后输出”，不留下静态窗。工作区：实时贴图参与“全部隐藏／关闭”（不另建一套列表），但**不参与跨进程恢复**（HWND 无法跨重启）；退出时可选择按冻结帧或最后单帧落为静态记录（待拍板）。

单帧抓取顺序：WGC 单帧（需要增加 Cargo feature；后台线程；自建 D3D 设备，或复用 `recording_gpu::create_video_device` 的写法）→ `PrintWindow(PW_RENDERFULLCONTENT)`（手写 extern；检测近全黑；**不做** TOPMOST 补救）→ 冻结选区像素，并提示“画面可能不是最新”。

## 5. 实现切片

所有新文件都要在 `lib.rs` 加一行 `mod`。`lib.rs`、`preferences.rs`、`capture_view.rs`、`Cargo.toml`、工具栏注册属于共享入口，正被其它会话修改，统一由一人在切片 4 合入；切片 1–3 期间只用 `#[cfg(test)]` 或 smoke 开关接线。

1. **纯模型 `src/pin_live_model.rs`（新建）**：源身份（hwnd/pid/tid）；选区全局矩形→客户区相对矩形的求交；由窗口矩形、扩展边框、客户区原点推算 `rcSource`；缩放和透明度沿用贴图规则；贴图点→源客户区→（按源窗口 DPI 感知）投递坐标；状态机（Live/Minimized/Cloaked/Lost/Unavailable→提示与可用操作）；交互与穿透互斥。验证：`cargo test --lib pin_live_model`。
2. **DWM 缩略图原生层 `src/pin_live_dwm.rs`（新建）+ `src/pin_live_smoke.rs`（新建）**：`Thumbnail` RAII 封装（`DwmRegisterThumbnail`／`DwmUpdateThumbnailProperties`／`DwmQueryThumbnailSourceSize`／Drop 时 `DwmUnregisterThumbnail`），手写 extern；源窗口探测（`IsWindow`／`IsIconic`／cloaked／矩形／`GetDpiForWindow`／感知方式）。smoke 用**自有**纯色 Win32 源窗口加自有宿主窗口，验证 DComp 叠放、layered 与缩略图透明度、`rcSource` 偏移、最小化和遮挡表现；像素只读自有窗口区域。验证：`cargo check` 后运行 smoke（smoke 分发注册属于共享入口，可先放在 `#[cfg(test)]` 的 ignore 测试里手动跑）。
3. **实时贴图视图 `src/pin_live_view.rs`（新建）+ `src/pin_live_snapshot.rs`（新建）**：GPUI 窗口（复用 `pin_native` 的 place/topmost/click_through 和 `pin_drag`），内容矩形挂缩略图，边框和状态画在内容外；4–10Hz 源窗口轮询；右键菜单；单帧抓取链（先只做 PrintWindow 加冻结像素回退，WGC 留给后续小切片，因为要改 `Cargo.toml`）；“转为静态”调用现有图像贴图打开函数（只调用，不修改 `pin_view.rs`）。验证：`cargo test --lib pin_live`；真实界面用 `flicker-probe`／自有源窗口，未验部分写“真实界面未验”。
4. **入口合入（共享入口，单人负责）**：`capture_view.rs` 增加 `Action::WindowPin`（建议放在贴图按钮的二级菜单或 `Shift+T`），`capture_pin_output.rs` 增加 `window_pin_started`：优先用吸附的 `WindowTarget`，否则在冻结窗口列表中取包含选区中心、且客户区与选区相交的最上层非本进程窗口，并做 pid/tid/bounds 身份复核；选不到就回退为普通贴图并给出提示。另改 `lib.rs`、i18n 文案、工具栏注册、`docs/behavior.md`；如需开关，改 `preferences.rs`。验证：`cargo check` 加 `cargo test --lib capture_pin`。
5. **二档转发 `src/pin_live_input.rs`（新建）**：源窗口子树内深层命中（`ChildWindowFromPointEx`，不用 `WindowFromPoint`）；`PostMessage` 投递 `WM_MOUSEMOVE`／`WM_LBUTTONDOWN`／`WM_LBUTTONUP`／`WM_MOUSEWHEEL`，按源窗口 DPI 换算坐标；不在 UI 线程 `SendMessage`；完整性级别更高的源窗口禁用转发；后台 MTA 线程复用 `control_selection` 子树命中，走 `InvokePattern`／`ScrollPattern` 兜底。验证：纯映射用 `cargo test --lib pin_live_input`；自有 Win32 listbox/edit 夹具的接收计数放在 smoke 里测。三档键盘待拍板后再切。

不改 `pin_view.rs`、`pin_presentation*.rs`（实时贴图不复用矢量文档呈现）；不改 `live_capture.rs`、`capture_frame_dxgi.rs`、`recording_gpu.rs`。性能口径：不碰托盘→冻结的启动路径，只在点击“窗口贴图”时做 HWND 解析；待命进程不受影响。

## 6. 已确认的范围

1. 用户确认输入转发实测没有明显价值，首版只看不点；滚轮用于贴图缩放。
2. 性能优先：持续画面由 DWM 合成，WGC 只在复制、转静态或关闭保存时抓取一帧，不逐帧读回。
3. 退出或重启尽可能恢复：实时贴图注册在现有工作区，冻结图先存静态记录，正常关闭时尽量更新为单帧快照；恢复得到普通静态贴图。
4. 平台边界：Windows 原生实现放在 `pin_live_dwm` / `pin_live_snapshot`，区域换算与状态放在 `pin_live_model`。macOS 支持独立窗口采集，可用 [ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit/sccontentfilter) 实现另一个后端；当前项目仍只支持 Windows。

## 当前实现与边界

Shift+T／更多菜单锁定源窗口与客户区相对区域；`pin_live_view` 使用独立原生宿主承载缩略图，避免 GPUI 与 DWM 内容叠放问题。Windows 窗口过程只转发事件；源身份、尺寸、最小化和主题以 200ms 间隔核验，持续显示没有像素读回。

工作区先保存冻结图，正常关闭或转静态时由后台线程运行 WGC 单帧，最多等待 500ms，只读回 ROI；不可用则沿用冻结图，转静态时明确提示画面可能不是最新。首版省略可能阻塞源应用的 `PrintWindow` 回退。转静态沿用同一 Session，实时 HWND 不跨重启恢复。

冻结后源窗口移动或变尺寸时要求重新框选；切换工作区组会同步显隐，托盘恢复交互不会被旧穿透状态覆盖。待命进程正常退出存在贴图时，先等待异步单帧保存，由最后一张贴图结束主循环。

区域与状态模型 4 项通过；显式自有窗口专项的隐藏注册／更新／释放、可见源尺寸、裁剪合成、Layered 透明度与 WGC 单帧 ROI/BGRA 均通过。隐藏且未呈现的 STATIC 窗口在本机返回占位缩略图尺寸，实际源尺寸合同由可见专项验证。

源窗口被遮挡／受保护、混合 DPI、最小化后恢复与退出更新最新帧的真实应用场景仍待验。自有窗口专项分别覆盖隐藏窗口 API 生命周期和可见合成／透明度；后者无安全桌面位置时明示跳过。
