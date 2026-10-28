# 选区确认后工具栏延迟修复（2026-10-12）

用户报告框选结束后工具栏出现过慢。普通 GPUI 的 `CaptureView::up` 已在结束手势时更新工具栏显隐并立即通知；没有工具栏专用等待定时器。首轮框选使用原生冻结首屏，工具栏由随后创建的 GPUI 编辑界面提供。

## 定位与改动

原默认路径在原生首屏完成冻结、绘制、DWM 提交和输入焦点确认后，先完成同源 DDB 的 CPU 读回、分屏与预览准备，才在主 STA 初始化 GPUI、驱动和字体。二者串行，快速完成框选时仍需等待后续准备。

`gpui_capture.rs` 改为在准备线程完成原生真实 Ready 后发送一次来源窗口通知。主 STA 此时初始化 GPUI，准备线程同时从本请求的原 DDB 读回像素。完整 `PreparedCapture` 回收并验证后才创建 GPUI 截图窗口。Pending 捕获对象从创建到消费始终留在同一线程，不跨线程转移 HDC 或 immediate context；没有重截桌面。原有完整 Present、DWM、物理客户区、输入焦点与 `claim_gpui_open` 合同保留。

新增 `capture_preparation.rs` 负责一次 Ready 通知与线程回收。没有原生 Ready 的回退、失败和 panic 等待完整任务终态；真实错误在初始化前返回。Ready 后的取消与失败同样回收原任务；初始化退出时 Drop 也等待清理，不留下脱离会话的冻结线程。显式 `RUSTSHOT_STARTUP_SERIAL=1/0` 保留原串行／完全并行对照，默认使用 Ready 门。

`fast_capture.rs` 只转移已预建显示设备线程的通道、JoinHandle 与原计时，准备线程的 TLS 仍由原生产路径消费这些 secondary worker。没有移动 HDC、已选位图、主线程 PRIMED_GDI 或 Pending 捕获对象；失败／退出沿原 DeviceTask Drop 回收。单屏主线程 PRIMED_GDI 留在原线程清理，其 HDC 在新准备线程中不复用；诊断 `DEVICE_BLACK_DDB_WARM=1` 的 main 同线程暖身同样不授予原线程复用资格。实际启动影响待自然使用验证，不能称所有预热原样保留。

当前 `target/qa/tray-run` 三件程序为 Debug，GPUI 路径每次新工作进程创建 renderer 时都会现场编译内置 HLSL。Release 原本已使用构建时生成并嵌入的 DXBC，本次交付沿用该路径；未改写并行 renderer 修复。现场编译是额外成本，但本轮未测量它对真实鼠标抬起到工具栏出现的具体贡献。

实际后台小探针直接调用当前原 `build_shader_blob`：10 模块／20 入口连续三批为 **952.3663、1005.7690、946.4668 ms**。其中 RoundedContentGroup/Fragment 单入口为 651.0456／703.6821／649.6044 ms。编译与运行退出均 0，源与 HLSL 前后 SHA 稳定，见 `shader-cost/verification.json`、`entries.csv`。读取在用 EXE 字节又核三个 HLSL 完整原文均已内嵌（`installed-shader-source.json`）；这是 HLSL 字节对应，不冒称整个旧 EXE 来源快照完整。该探针没有窗口、设备、桌面或剪贴板；这是编译函数成本，不能记作工具栏首显改善了同样的毫秒数。

## 验证与交付边界

证据目录 `target/qa/toolbar-latency-20261012`。固定来源为 `continuous-development-20261010/product-20261012-142457-6402723/source` 的两源审阅后继；原复制回执与在途首轮编译保持历史，最终资格由最终固定来源重新执行取得。

七项定向模型通过独立 `rustc --test` 直接导入实际冻结生产模块执行，7/7 通过；覆盖 Ready 提前放行且同源准备尚未结束、无 Ready 回退只能在终态继续、Ready 前后取消标志／失败信息的运输、Ready 前后 panic、Receiver 关闭、一次通知与退出回收。该模型范围不冒称已执行 Win32／DXGI 取消分类器或真实窗口。两次整库模型编译已由本任务停止，不授予模型资格，见 `cargo-models-interrupted.json`；在途首轮不能作最终来源资格。

候选首次全库格式检查失败于已有并行源码；只在固定 QA 来源执行 cargo fmt，并删除未调用的图片翻译 `Skip::label`。后继 strict 先后指出冗余闭包和测试的单 Range 数组；只在 QA 改为原函数直接传入和 `slice::from_ref`，原范围／断言保持，主目录这些源码保持原样。三个初始失败按原回执保存。

最终格式检查和全目标 `cargo clippy --all-targets --locked --offline --jobs 1 -- -D warnings` 已自然 0，完整 817 来源稳定，见 `format.json`、`strict.json`。这是当前生产入口与所有测试目标的类型／lint 资格；七项执行资格仍来自实际原模块测试。原 `build.ps1 -Jobs 1` Release 三件套已自然 0、完整来源稳定，交付 CLI `--help` 实际 0。三个相关生产源码与最终 QA 来源逐字节相同（`purpose-source-identities.json`）。

完整交付目录 `dist/local/RustShot-toolbar-latency-20261012` 与同名 ZIP 已生成。三 EXE 均为本次新生成、复制 bytes／SHA 一致；GPUI EXE 含新增初始化门标记，实际 PE 导入不含 D3DCompile／D3DCompileFromFile。包包含四件 app-local VC++ 运行库、按固定清单核 bytes／SHA 的四件 ONNX Runtime／许可及中文使用说明；ZIP 的 12 个成员又逐件核大小／SHA 一致。见 `final-verification.json`。最终检查脚本曾因 ConvertFrom-Json 的 DateTime 被隐式文本化后重复转换 UTC 而误判 freshness，仅修脚本后通过；程序与实际文件时间／哈希均未因此改变。

没有显示测试窗口、读取／上传桌面或剪贴板，也没有自动替换当前运行实例。真实 GPUI smoke、多屏 DPI、鼠标抬起到工具栏真正可见的时间和本轮优化收益待自然使用验证，不能以并行模型或构建耗时代替。长期整体目标未完成，无 commit／push。
