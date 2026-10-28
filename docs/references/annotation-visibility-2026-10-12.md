# 编辑标注预览不可见修复（2026-10-12）

用户报告全屏、图片和贴图编辑中的元素不可见，但复制图片仍含元素。问题位于预览 GPU 路径，标注文档和 CPU 输出没有丢失。

## 原因与修复

GPUI 的 Debug 着色器原通过 `D3DCompileFromFile` 在启动时读取编译目录中的 HLSL。旧程序复制到其它目录后仍读取原工作区；源码继续迭代会使旧 Rust 缓冲布局与新 HLSL 不匹配。本次旧 `PathSprite` 为 80 bytes，新版因增加 `intermediate_bounds` 为 96 bytes；新着色器把旧路径的空画刷区域读成中间纹理区域，纹理坐标的分母为零，最终标注路径不可见。底图、普通矩形控件与 CPU 导出仍正常。

当前在用 `target/qa/tray-run/kirie-gpui.exe` 为 Debug，SHA256 `E112A85E900ED56FA2448DDC30A6FD066457B25EFCD26A16DCA12960CDDCE5CA`；与当时 `target/debug` 相同，字节中没有新增整组 renderer 的错误常量。没有这件 EXE 的完整来源快照，不能将其冒称当前源码同源构建。旧布局的本机 GPUI 库在隐藏窗口中实际复现两种路径均消失，匹配布局恢复，支持上述诊断。

修复 `vendor/gpui/src/platform/windows/directx_renderer.rs`：Debug 将主 HLSL、Emoji HLSL 和 alpha helper 用 `include_str!` 固定到编译产物，通过 `D3DCompile` 编译内存源码，不再读取工作区文件。入口、shader model 和调试编译参数保持原合同；Release 仍使用构建时生成的 DXBC。依据 [Microsoft D3DCompile 文档](https://learn.microsoft.com/en-us/windows/win32/api/d3dcompiler/nf-d3dcompiler-d3dcompile) 检查源码长度、诊断文件名与 include 参数。

## 后台证据

证据目录：`target/qa/annotation-visibility-20261012`。窗口均 `show=false`、`focus=false`，只绘制深色合成像素，并核验不可见且没有取得前台；未读取桌面或剪贴板。

- `legacy-path-run.log`：旧 renderer＋当前工作区 HLSL，控制 Quad 为 `[0,221,0,255]`，普通路径／union 路径均为底图 `[31,35,40,255]`，实际复现。
- `path-run.log`：匹配的 renderer＋HLSL，两种路径均为预期红色 `[0,0,255,255]`。
- `wide-current-renderer-run/report.json`：实际生产 `CaptureView` 的截图、图片编辑、styled Capture Pin，各自底图／矩形拖动／矩形提交／画笔拖动／画笔提交，15 个阶段均通过。矩形目标区域可见红色至少 4219 像素，画笔目标区域可见青色 844 像素。
- `embedded-renderer-run/report.json`：修复后的内置 HLSL renderer 再次执行同样 15 阶段，全部通过。旧在用 EXE 导入 `D3DCompileFromFile`，修复探针导入 `D3DCompile`，用实际 PE 导入表核对；并非只改文档或编译配置。
- `normal-path-run.log`：正常 Cargo 优化的 GPUI 库再次运行隐藏 GPU 探针，普通／union 路径均为预期红色，窗口不可见且未取得前台。正常交付库的路径像素已实际验证。
- `shader-source-absent-test.log`：从实际生产模块提取的新增测试真实执行 1 项，内部覆盖 10 个模块的 20 个 shader 入口。编译测试 EXE 后移走仅 QA 目录的三个 HLSL 文件，仍全部编译通过；随后恢复原文件。生产 HLSL 从未移动或替换。

初次探针因隐藏窗口尚无有效客户区失败，后继明确设置隐藏 Popup 的物理客户区并等待 WM_SIZE；初次 640×480 画布中部分测试 ROI 被真实工具栏覆盖，扩大自有隐藏客户区后保留原业务与像素断言并通过。工具输出与 false 回执记录这些失败，不能把夹具失败当产品故障。

手工隔离编译 GPUI 后的第一次探针没有链接 Cargo 原有 Common-Controls v6 manifest，Windows loader 返回 `0xC0000139`，尚未进入绘制；补链接实际 `gpui.manifest.xml` 后执行上述内置 renderer 15 阶段。产品继续使用原 Cargo/build 流程，不修改其 manifest 或控件库合同。

上述验证证明自有隐藏窗口的 GPU 像素与编译时固定着色器，不代替用户桌面冻结、真实鼠标操作、DWM 首显／焦点、多屏 DPI 或启动性能验证。

## 本机交付

正常 Debug 首轮 `cargo build --locked --bins --jobs 1` 因构建期间新增 `run_qr_smoke` 入口而报 E0425，原 `product-build.log` 保留。后继正常 Cargo PID 156532 实际退出 0；三件程序于 11:36:53–11:36:59 UTC 完成，全部固定到 `candidate`，交付相关源码与已验证渲染输入的 SHA 均保持一致。没有使用优化级别为 0 的探针作为日常程序，也没有撤回并行新增的 QA 入口。

实际新 GPUI EXE 导入 `D3DCompile`，没有 `D3DCompileFromFile`；原四项 GPU／字体 DLL 延迟加载仍保留。新 CLI `--help` 在后台实际退出 0。程序、库与校验身份分别见 `normal-build-result.json`、`normal-library-artifacts.json`、`candidate-binaries.json` 和 `candidate-cli-check.json`。

11:39:29 UTC 已替换 `target/qa/tray-run` 的三件程序。旧版备份于 `previous-runtime`；关闭前正向核验原实例只有未消费的 IPC v2 IDLE 待命槽，没有活动截图、编辑或 Pin，也没有待自动恢复的可见贴图，随后请求原托盘正常退出。检查是瞬时状态核验，不冒称旧版具备全部启动入口的原子退出协议。没有修改旧 IPC 状态或直接结束其它实例。

新托盘 PID 28676、其新待命进程 PID 88864 已实际启动；新控制窗、owner/token、IPC IDLE 与 DWM 隐藏预热表面核验通过，在用三件与候选逐件 SHA 相同。新 `kirie-gpui.exe` SHA256 为 `C269AD32D85B5930058E954EB8CED6840431A706EDC66D67EB91E552CF928332`。另一个 PID 113108 实例保持原样。下一次从当前托盘／快捷键进入编辑使用新版；既有其它实例不获此次运行身份资格。最终回执见 `runtime-update.json`、`new-runtime-state.json` 与 `verification.json`。未安装、上传、发布、commit 或 push；不扩大真实交互／首显／性能资格。
