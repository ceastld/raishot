# GPUI 本地依赖

2026-10-12 修复 Debug 编辑标注不可见：Windows 调试版原在启动时从工作区读取 HLSL，旧 EXE 会与继续开发后的新缓冲布局混用。现在主着色器、Emoji 着色器及 alpha helper 均随编译产物固定，用 `D3DCompile` 编译内存源码；Release 仍使用构建时 DXBC。旧 renderer 与新 HLSL 的隐藏 GPU 探针实际复现路径不可见；匹配来源的三类画布 15 阶段通过，内置源缺文件测试实际覆盖 20 个 shader 入口。详见 [标注可见性修复](../references/annotation-visibility-2026-10-12.md)。

Debug 贴图窗口初始化补丁位于 `gpui/build.rs` 和 `gpui/src/platform/windows/directx_renderer.rs`：SDK 的 FXC 可用时，以 `/Od /Zi` 在构建期预编译全部 20 个入口，首次截图和后续贴图均直接使用同次构建字节码；三份 HLSL 与 FXC／PATH 环境变更会触发重新生成。无 SDK 时仍编译内置源码，按模块与 Vertex／Fragment 入口在当前进程缓存成功的不可变字节码，失败允许重试。每窗口继续独立创建 shader 对象与可变缓冲；Release 保留 `/O3`、缺 FXC 或编译错误时失败的语义。

普通彩色图块的线性采样补丁（2026-10-22）位于 `shaders.hlsl`：fragment 采样坐标钳在当前图块首末 texel 中心，避免放大外缘混入 atlas 邻图或空隙，保留内部线性过滤。设计与验证边界见[图片放大边缘](../references/image-animation-streams.md#放大边缘采样后继)。

高亮图像画刷补丁位于 `gpui/src/scene.rs`、`window.rs`、`platform/windows/directx_renderer.rs` 和 `shaders.hlsl`。Windows 新增 `Window::paint_path_image`：原始底图的局部颜色纹理与矢量路径分开，沿用解析边缘和 MSAA，缩放时重新绘制路径。图像画刷各自成批，保持对象绘制顺序；颜色纹理由现有 atlas 缓存，绘制后解绑额外 SRV。分块颜色带一像素邻接边，双线性采样连续；分块边界只裁最终矩形，不裁路径覆盖率，避免分数缩放的接缝重复叠加透明度。截图画布同时使用新增 `paint_image_exact` 保留底图分数坐标，避免取整后的底图与矢量标注在缩放时错位。普通 `paint_path`、`paint_image` 保留原有语义；该扩展仅提供 Windows API，未实现其它平台的图像路径画刷。

本轮实测发现，AA 三角形在 MSAA 中间掩模阶段采用 source-over 并提前带入对象 opacity，会使同一笔迹的重叠边缘重复着色、自交处加深。已以显式 `coverage_union` 标记作限定范围修复：普通 GPUI 默认 Path 行为保持不变；RustShot 的单条 union 矢量路径独立成批，在 MSAA 中间面写白色不透明覆盖率，以 MAX 合并三角形覆盖，最终阶段才一次应用对象 color/opacity。Multiply 图像画刷同样先合并 mask，再将原图乘色结果合成一次。修复仅限 Windows，保留 2048 像素互不重叠 core、1 像素 halo 和 `paint_image_exact` 的原有坐标合同；底图预览、矢量画刷和叠加层显式共用精确 viewportBounds，避免 Taffy 圆整底图引入采样错位。

Windows 增加一次性 GPU ROI 读回供显式请求验证：仅请求时分配临时 staging，按物理客户区 ROI 拷贝，从 Map 的 RowPitch 逐行读取并 Unmap；默认渲染不读回，也不持有常驻读回纹理。结果是预乘 BGRA 的 GPU 帧缓冲，不是 DWM 最终桌面合成结果。Debug 的 4 组自有真实 GPUI Window 专项通过（`target/qa/highlighter-native/20261003-065708-640/summary.json`，`pixel_backend=gpu_framebuffer`、125% DPI、0.37 缩放分块最大误差 1，输入为自有 HWND 的 PostMessage）；DWM 桌面最终取样因外窗遮挡未完成，前台恢复、混合 DPI、真实 IME 不在此证明范围内。最终严格检查、Release 构建与三件套交付已完成。

`gpui/` 基于 crates.io 发布的 GPUI 0.2.2，保留框架本体、构建脚本、着色器和测试；示例及示例资源不进入公开源码，两个 manifest 已移除显式示例入口。不修改用户 Cargo registry 缓存。

- 发布包：`gpui-0.2.2.crate`，SHA-256 `979b45cfa6ec723b6f42330915a1b3769b930d02b2d505f9697f8ca602bee707`。
- `gpui/LICENSE-APACHE` 保留 Zed Industries 的版权及 Apache-2.0 全文；源码版权头、`Cargo.lock` 和 `.cargo_vcs_info.json` 保留。
- 项目根 `Cargo.toml` 使用 `[patch.crates-io]` 指向 `vendor/gpui`；版本仍为 0.2.2，不新增运行时依赖。

设备复用补丁位于 `gpui/src/platform/windows/directx_devices.rs`。原路径在选择适配器时调用 `D3D11CreateDevice`，真实创建并验证一个设备后释放，再为同一适配器重新创建正式设备。新路径按同样顺序枚举适配器，保留第一次通过验证的设备、immediate context 和 feature level。仍使用原创建 flags、11.1/11.0/10.1 feature level 列表、StructuredBuffer 功能检测和不支持适配器的跳过逻辑；设备丢失后的恢复继续走 `DirectXDevices::new()`，不跨截图会话或进程保留 GPU。

字体准备补丁位于 `gpui/src/platform/windows/direct_write.rs` 和 `platform.rs`。平台主线程仍调用 `OleInitialize`，独立短线程配对调用 `CoInitializeEx(MTA)` / `CoUninitialize`，仅创建共享 `IDWriteFactory5` 和系统 `IDWriteFontCollection1`，与主线程创建 D3D 设备重叠。当前 `windows 0.61` 生成绑定明确将这两个系统接口声明为 `Send + Sync`；GPUI 的 `PlatformTextSystem` 也要求 `Send + Sync`，既有字体状态通过锁共享。可变字体 builder/loader、自定义文本 renderer、GPU 状态和 immediate context 都留在主线程，准备线程没有设备参数。

字体集合继续调用 `GetSystemFontCollection(false, ..., true)`，每次会话立即检查安装变化；未改为延迟检查的缓存路径，语义见 [Microsoft 参数文档](https://learn.microsoft.com/en-us/windows/win32/api/dwrite_3/nf-dwrite_3-idwritefactory3-getsystemfontcollection)。线程创建失败、COM/字体错误和 join 捕获的 panic 均回到原主线程字体初始化；即使 D3D 初始化失败也会先 join 字体任务，后台不跨平台生命周期存活。设备丢失仍只重建原 GPU 状态，不再启动字体线程。

路径纹理补丁位于 `directx_renderer.rs`。未选区截图首帧只有冻结图像、矩形遮罩和文字，不需要路径中间面；现在仅在首个非空路径批次之前创建普通路径纹理与 4 倍 MSAA 纹理，同尺寸后续绘制复用。窗口尺寸变化解绑 VS/PS 的旧纹理引用后失效缓存，资源恢复后重新按需创建。像素格式、采样数、ResolveSubresource 与两阶段绘制保持原样。4480×1440 下两张路径纹理的逻辑像素载荷约 123.05 MiB；这不是全部 GPU 内存或进程工作集的测量值。

局部路径中间面补丁（2026-10-15）同在 `directx_renderer.rs`。RustShot 每条矢量标注都是独立的 `coverage_union` 批次，原先主窗口上每批都要清空并 resolve 整窗 4×MSAA 中间面，标注越多每帧成本随“笔数×窗口面积”线性增长，几十条线后拖动明显卡顿。现在主窗口（非圆角组）路径批次先求可见边界，按 2 的幂分档（最小 256）使用局部 MSAA/resolve 纹理，绘制时把目标原点设为边界左上角，与圆角组分块共用同一着色器约定；分档面积达到整窗一半时仍用整窗中间面。分档纹理最多缓存 4 张，resize 和设备恢复时与整窗路径纹理一起丢弃；完全在窗口外的批次直接跳过。圆角组内的路径分块逻辑不变。

保留层补丁（2026-10-15）位于 `scene.rs`、`window.rs` 和 `directx_renderer.rs`。新增 Windows API `Window::paint_retained_layer` 与 `RetainedLayer`／`RetainedPaint`：内容画进跨帧保留的纹理，之后 `Reuse` 只合成、`Append` 只追加新内容。实现上它是带 `retained` 请求的圆角内容组（零圆角、无 mask），结束时按整数 ROI 原样拷贝。整组画完才登记内容键，请求 id 让重复 Present 同一 Scene 时幂等；帧末释放未使用的纹理，resize／设备恢复全部失效；内容不可用时放弃该帧并投递强制重绘。圆角组内路径中间面改为按 2 的幂分档分配（不超过组本身），重建几十笔时不再逐笔创建 MSAA 纹理。普通圆角组和主窗口路径行为不变。策略与测量见 [标注渲染缓存](../references/annotation-render-cache-2026-10-15.md)。

相关资源失败路径使用可选句柄和普通 RAII 局部，完整构造成功才安装。resize 失败不提前更新有效尺寸；设备资源恢复先释放旧 HWND 的交换链/composition 归属，保留合法空槽及尺寸、渲染策略，再构建新资源，允许失败后重试，避免访问和再次析构已释放的结构。

快速框选交接补丁位于 `window.rs`、`events.rs`、`platform.rs` 和公共 `src/window.rs`。Windows renderer 成功完成 draw/present 后才递增该 HWND 的呈现序号，并记录实际渲染尺寸；原生冻结首屏在 GPUI 导入选区后的新帧呈现成功、客户区/视口/DPI 校验及 DwmFlush 完成后才撤下。绘制失败不会被当成完成，等待至多两秒再取消本次会话。窗口原生属性只保存整数，不保存图像或跨会话缓存。

初建隐藏窗口已经由原生显示时，后续激活会丢弃过期的 Windowed 初建位置，保留实际全屏范围；隐藏的正常打开和 Maximized/Fullscreen 状态保持原语义。Windows 原生消息快照函数复用已有键盘布局和修饰键解析，供临时首屏在消息到达时保存输入；主线程使用 GPUI typed 事件重放，避免历史 Ctrl/Shift 被解释成当前已释放的按键状态。未使用 SendInput 或修改系统键盘状态。公开既有 `dispatch_event` 的结果类型，让接入层可以调用该方法；没有改变其派发实现。

Windows 光标补丁位于 `platform/windows/events.rs`。光标变更消息按实际句柄判断变化，在两种可见光标之间切换时也立即调用 `SetCursor`，覆盖鼠标捕获期间不会收到 `WM_SETCURSOR` 的情况。消息仍更新每个窗口的缓存，但只有持有本线程鼠标捕获、或经 `GetCursorPos` / `WindowFromPoint` 确认指针实际位于其 HWND 的窗口才应用系统光标；本线程其它窗口及经 `GetGUIThreadInfo(0)` 观察到的前台其它线程捕获会阻止更新，避免后台广播抢光标。窗口初始化及唯一光标变更写点同步更新独立 `Cell<Option<HCURSOR>>` 镜像；激活和 `WM_SETCURSOR` 从镜像复制句柄，因此窗口状态正在借用时也能重申所属光标。原生光标更新不借用 RefCell、不调用 App 回调，合法 `None` 和原有指针归属保护保持不变。

`platform/windows/window.rs` 的窗口类使用普通箭头作为尚无窗口状态时的默认光标。激活消息立即重申所属窗口的缓存光标，App 激活回调仍延迟执行；RustShot 的 `startup_handoff.rs` 在原生首屏成功销毁、所有 App/Window 借用结束后，向仍为前台且实际命中指针的 GPUI HWND 同步发送客户区 `WM_SETCURSOR`，避免静止指针在交接时保留忙光标。合法 `CursorStyle::None` 保持隐藏语义，RustShot 的移动和对角角点仍使用既有矢量覆盖层，不能仅凭系统光标不可见判定自绘像素已验证。原生首屏另在焦点就绪时设置已注册的十字光标，活动客户区继续响应 `WM_SETCURSOR`；这些改动不改变进程启动反馈或真实首屏性能验收口径。

## 同版本对照

仅用于 QA 的环境变量：

- 默认使用复用路径。
- `RUSTSHOT_GPUI_LEGACY_DEVICE_PROBE=1` 恢复原“探测后释放，再正式创建”路径。两种路径可以运行同一个 Release 二进制。
- `RUSTSHOT_STARTUP_TRACE=1` 才输出 JSON 初始化记录；默认不输出。`gpui_device_mode` 给出 `reuse` / `legacy_probe`，`startup_stage` 增加 `gpui_dxgi_factory`、`gpui_adapter_selection`、`gpui_directx_init` 及逐次 `gpui_d3d11_probe_create` / `gpui_d3d11_device_create` 与相应 validation 耗时。
- 默认将系统字体准备与设备创建并行；`RUSTSHOT_GPUI_SERIAL_FONTS=1` 保留原主线程串行字体路径。`gpui_font_mode` 在字体系统初始化完成后报告实际 `serial` / `parallel` / `fallback`，发生回退的样本不能当作并行收益。
- 字体细分 `startup_stage` 包括 `gpui_font_factory`、`gpui_font_collection`、`gpui_font_gpu_initialize`、`gpui_font_text_initialize`；并行额外记录 `gpui_font_cpu_prepare` 与主线程 `gpui_font_join` 等待。各子阶段不能直接累加为端到端耗时，因为 CPU 字体与设备创建重叠。
- `RUSTSHOT_GPUI_GPU_PREFERENCE=minimum_power` 让 `directx_devices.rs` 用 `EnumAdapterByGpuPreference(MINIMUM_POWER)` 按省电顺序选适配器（混合显卡时核显在前）；默认仍按 `EnumAdapters` 顺序。追踪开启时 `gpui_adapter` 报告实际选中的适配器。用于托盘弹层冷启动对照，见 [托盘弹层](../references/tray-popup-2026-10-24.md#10-冷启动拆分与预热决定10-24)。
- `RUSTSHOT_GPUI_EAGER_PATHS=1` 保留原窗口创建/resize 时预分配路径纹理的方式。默认按需分配；`gpui_path_mode` 报告实际 `eager` / `lazy`，`gpui_first_draw` 记录首帧路径数与是否已有纹理，`gpui_path_resources_create` 记录尺寸、逻辑字节数及分配耗时。renderer 创建、管线、composition、字体参数和每次 resize 分别计时。
- `RUSTSHOT_GPUI_RESOURCE_RECOVERY_PROBE=1` 只用于独立合成 paths/quads 专项：模拟资源构造中途失败，再用相同设备句柄重试，检查空槽、回收及绘制。它不模拟实际驱动移除，也不证明硬件热插拔恢复。带旧 atlas 图像/文字的场景会拒绝此探针；正常使用没有探针输出。

在首个适配器可用时，D3D 设备创建及功能检测应从各两次减为各一次；具体毫秒收益须通过真实机器测量。对照时交替运行两种模式的新工作进程，比较 GPUI 初始化、创建到首帧及完整托盘触发延迟，确认取消后进程和截图互斥体释放。不得把上层 GPUI 初始化时间的变化当成所有机器必然得到的收益；帧呈现、采集、进程创建仍各有成本。

字体对照固定设备、采集和窗口几何优化，轮换执行 `Tools/measure-startup.ps1 -CompareFonts` 的串行/并行路径。并行收益取决于实际字体 CPU 准备耗时与设备初始化的重叠量；不能将未拆分的 GPUI 额外初始化耗时全部当作字体成本，也不承诺固定毫秒收益。确认中文/英文、标点与符号渲染、窗口首帧和取消回收仍正常。

路径资源对照使用 `Tools/measure-startup.ps1 -ComparePaths`，固定已有设备/字体/截图/窗口优化，轮换 eager 与 lazy 的新进程。只有首帧没有路径、实际分配模式正确，且窗口几何、视口、取消和回收都通过时才统计。随后使用 `--path-resource-smoke` 检查首次路径的 AA/透明度、真实 resize 后的新旧路径与原生关闭；仅读取自有合成客户区小块。

显式启动追踪还拆分 Windows 窗口创建、主题、拖放注册、放置，以及 App 初次场景构建。`directx_atlas.rs` 仅在大型底图首次进入 atlas 时分别记录分配与上传；不复制图像，不改变缓存命中或上传行为。没有追踪变量时不输出这些记录。

本地补丁保留文本布局/回退字体、着色器、DPI、适配器优先级和 MSAA 质量，改变路径纹理分配时机并修复相关释放失败路径。后续升级依赖时从新发布包重新导入，复核这些文件的差异、接口线程约束和发布包校验值，再进行同版本 A/B 与真实 UI 验收。

输入法关联补丁位于 `gpui/src/platform.rs`、`window.rs` 和 `platform/windows/window.rs`。`PlatformWindow` 新增默认空实现的 `set_ime_enabled`，`Window::draw` 在登记输入处理器后按本帧是否有获得焦点的输入处理器调用。Windows 仅在状态变化时调用：关闭时先以 `ImmNotifyIME(NI_COMPOSITIONSTR, CPS_CANCEL)` 取消未提交组字，再 `ImmAssociateContextEx(hwnd, NULL, 0)` 解除关联；开启时用 `IACE_DEFAULT` 恢复默认上下文。目的是让中文输入法下的单字母快捷键直接作为按键到达，不触发组字。其它平台保持原行为。

`gpui/src/assets.rs` 新增消费接口 `RenderImage::try_into_single_frame`：仅在一个 frame 且单位 scale factor 时移出原 `image::Frame`，不复制像素 Vec，并保留 offset、delay 等元数据；拒绝时原样返回整个 `RenderImage`，包含原 ID 和全部帧。调用方先用 `Arc::try_unwrap` 判定独占，共享时保留原 Arc 并走借用路径；像素修改后使用 `RenderImage::new` 重建以生成新 ID。该接口不暴露共享的可变像素，也不改变现有构造及渲染行为。

WM_PAINT 清除补丁（2026-10-20）位于 `platform/windows/events.rs` 的 `draw_window`：原先只 `ValidateRect` 客户区，隐藏窗口更新区域里的窗框、待擦除或内部绘制部分不调 `BeginPaint` 就永不清除，系统每秒数万次重发 WM_PAINT，主线程空转在 `GetMessage` 内核里（长截图编辑期间隐藏的采集面板实测）。现改为 `RedrawWindow(RDW_VALIDATE | RDW_NOFRAME | RDW_NOERASE | RDW_NOINTERNALPAINT)`；vsync 失效与隐藏窗口预备帧的驱动方式不变。10-21 补：`RedrawWindow`／`ValidateRect` 对不可见窗口是空操作——窗口带着更新区域经样式位变为不可见时（实验：`ShowWindow`／`SWP_HIDEWINDOW` 隐藏会顺带清掉，直接去掉 `WS_VISIBLE` 样式位则不会），区域永远留着；清除后若 `GetUpdateRect` 仍非空，再以 `BeginPaint／EndPaint` 兜底，这是唯一对不可见窗口有效的方式。见 [长图编辑重写](../references/long-editor-redesign-2026-10-20.md#卡顿根因10-20-补)。

缩放先出帧补丁（2026-10-19）位于 `platform.rs`、`window.rs` 和 `platform/windows/{window.rs,directx_renderer.rs}`。`PlatformWindow` 新增四个默认空实现的方法，Windows 实现：`prepare_content_bounds` 按目标客户区物理矩形先调整交换链（失败不改有效尺寸），再更新逻辑尺寸与原点；`defer_next_present` 让下一次绘制只渲染不 Present，`present_deferred` 提交该帧（交换链重建会作废待提交帧）；`wait_for_gpu` 插入 D3D11 事件查询等 GPU 执行到此。推迟期间不递增 `RustShot.GpuiPresentSerial`，真正 Present 时才递增。公共 `Window` 上同名方法直通平台，`prepare_content_bounds` 之后走原 `bounds_changed`。用途：贴图改尺寸并移动时先按新尺寸画好，紧接一次合成后再 Present 并移动 HWND，之后的 WM_SIZE 同尺寸跳过重建。其它平台与默认窗口行为不变。设计与实测见 [贴图缩放](../references/pin-zoom-latency-2026-10-16.md#第三步先出帧再移动窗口10-19)。

## omnitrigger

`omnitrigger/core` 与 `omnitrigger/windows` 提供键盘／鼠标任意组合的确定性状态机、录制器与 Windows 低级输入 Hook 适配，供全局触发器使用（设计见 [全局触发器](../references/global-triggers-2026-10-22.md)）。源码为同一作者的 MIT 项目 0.1.0 版本原样拷贝，`LICENSE` 保留其版权与许可全文；只替换了两个 manifest（去掉工作区继承，`windows` 依赖与本仓库同为 `=0.61.3`），未改源码。`windows/src/title_bar.rs` 与提示窗口辅助函数为原库自带，RabShot 不调用。升级时整体替换 `src/` 后跑 `cargo test --lib trigger`。
