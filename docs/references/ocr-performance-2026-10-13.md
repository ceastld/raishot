# OCR 性能对比：SimdPaddleOCR 与现有 Rust + ORT（2026-10-13）

## 问题

用户要求研究 [sdcb/SimdPaddleOCR](https://github.com/sdcb/SimdPaddleOCR)（纯 C#，自带托管 ONNX 解释器和手写 AVX2/AVX-512/NEON 算子，Apache-2.0），对比当前 OCR 效率，评估是否值得用 Rust 重写一个同类推理引擎。另有 [mg-chao/simd-paddle-ocr-rs](https://github.com/mg-chao/simd-paddle-ocr-rs) 直接移植；此前“只有空初始提交”的记录已过时，最新核查见下文。

> 以下早期对照包含不同图片管线。当前结论以“完整 Rust SIMD 复刻与当前客户端同管线实测”一节为准，不能把早期 PW 局部提速或 DET 尺寸差异当成当前项目的收益。

## 冷加载提速与预热选项（2026-10-16，已实施）

用户反馈“偶尔 OCR 等很久，下一次又特别快”。冷加载是**每个新进程一次**的工作，命中以下窗口就整段等待，之后同进程立刻变快：

- **截图后不久再截图**：托盘在上一截图释放 `Local\RustShot.MVP.Capture` 后的 1 秒定时器里补建待命进程，新待命 WarmReady 后才开始 OCR 预热；窗口内发起的识别在 `ENGINES` 锁上等预热做完。
- **非截图来源的贴图宿主**（`--pin-file`／`--pin-clipboard`／`--pin-restore`／`--pin-workspace`／登录恢复）此前从不预热，首次识别现场冷加载。

**根因：ORT 1.24 创建全局环境时的 SetupAPI 设备枚举。** 冷预热分段（新进程）：三个会话构造合计仅约 0.1 秒，环境创建 4.1–7.2 秒且线程 CPU≈墙钟，是本线程纯计算。v1.24.4 的 `DiscoverDevicesForPlatform` 依次走 DXCore、SetupAPI（显示／处理器／计算加速器三类、读硬件 ID／名称／驱动版本）、DXGI，无跳过开关。单独复刻这些系统调用：SetupAPI 38 个设备约 1.3 秒 CPU，DXGI 约 45ms，DXCore 约 3ms；Python 直接调同一 DLL，把其 `SETUPAPI.dll!SetupDiGetClassDevsA` 导入换成返回空集合的桩，CreateEnv 1.34–1.50→0.14–0.16 秒。RustShot 只用 CPU EP，该清单无用；SetupAPI 枚举失败时 ORT 改由 CPUID 得到 CPU 设备，DXGI／DXCore 部分照常。`ort` 未启用 tracing、日志级别 ERROR，排除日志开销。

改动：

- `src/ppocr/discovery.rs`：加载并核对 1.24.4 版本后、首次创建环境前，带越界检查地解析已映射的 PE 导入表，只把 onnxruntime.dll 的这一项导入指向桩；找不到该导入或改写失败则原样走完整发现。测试进程可设 `RUSTSHOT_OCR_DEVICE_SCAN=full` 作对照（产品构建恒不读取）。
- 同机新进程冷预热 A/B（无编译重叠两轮＋重叠一轮）：完整发现 4.39／4.37／7.50 秒 → 跳过 0.29／0.29／0.42 秒，其中环境 4.2–7.2 秒 → 0.14–0.25 秒。合成集 100 张图、1104 行识别文字两种模式逐字一致。
- 真实托盘待命进程（`ocr-timing.jsonl`）：旧构建预热 4.82 秒（环境 4.69 秒、CPU 4.58 秒）；新构建 0.41 秒（环境 0.23 秒），进程启动后约 0.94 秒就绪。调试版 CLI 冷进程整次识别约 0.58 秒（推理约 0.22 秒）。
- 设置 ▸ 截图 ▸ 文字识别新增“预热”：截图时（默认，原行为）／始终（贴图宿主首显 1 秒后也预热）／关闭（省内存与截图后的后台加载）。键 `ocrWarmup=standby|always|off`。
- 诊断：预热、现场初始化、等锁 ≥100ms 或总耗时 ≥1.5s 的请求写一行到 `%LOCALAPPDATA%\RustShot\diagnostics\ocr-timing.jsonl`（资产／运行库／环境墙钟与线程 CPU／det／识别会话／推理／进程存活时长／是否跳过设备枚举；不含图像文字，200 行滚动）。环境单独调用 `Environment::current()` 只为计时，构造顺序不变。
- 曾试过初始化期间对线程请求 HighQoS：同条件交替三对仅 0–7% 且一对反向，已撤回，不作为提速手段。

复现：`cargo test --lib --no-run` 后直接运行测试程序 `ppocr::warmup::tests::cold_warmup_timing --ignored --nocapture --exact`，加或不加 `RUSTSHOT_OCR_DEVICE_SCAN=full`；结果一致性用 `ppocr::bench_tests::ocr_bench_dir` 的 `RUSTSHOT_OCR_BENCH_OUT` 两次输出比对。

未做：真实截图界面里的首次识别手感；换 ORT 版本时需重新核对导入名（找不到即自动回到完整发现，不会出错）。

## OCR 偶发慢与闲置恢复调查（2026-10-16）

本轮只增加独立实验和诊断，未替换生产后端或调整正在使用的进程。复用 `current-app` 的生产管线、同一 Tiny 资产与同一张 864×976 图片，记录资产校验、首次会话构造、推理、进程 CPU 时间、工作集和 page-fault 计数；所有重复输出完全一致。初步结果分清以下几件事，不能合并成“SIMD 不够快”。

### 已确认的初始化与生命周期

- `ENGINES` 是进程内缓存，无闲置 TTL；同一进程没有因经过十秒或一分钟自动清空引擎的代码。截图待命子进程被消费、截图结束退出后，后续待命是新进程，不能复用上一进程的会话。贴图可以保留旧宿主，命令行仍可能为 `--capture-standby`，不能据此判定泄漏。
- `WarmReady` 在后台 OCR 准备启动之前发出，表示截图待命可用，不表示 OCR 已经准备好。OCR 预热构造 det／cls／rec 与字典，不执行模型 Run；新待命进程很快接到 OCR 请求时仍可能等待初始化。
- 无编译重叠的六轮中，资产 SHA 校验约 28ms；首次 `CpuSession::new` 为 5.55–8.53 秒，后两个为约 22–86ms。这里首次会话一直是 det，**不能把整段耗时称为检测图优化**。Rust `ort` rc.13 的 `init().commit()` 只保存配置，第一次 `Session::builder()` 才实体化全局 Environment。
- 直接调用同一 DLL 的独立新进程试验，把 det／cls／rec 加载顺序互换：CreateEnv 本身约 1.4–3.1 秒，之后 CreateSession 为几十到一百多毫秒。该批次有其它会话编译重叠，只用于核对调用边界，不能与上述 Rust 冷启动直接相减或宣称已解释全部 6 秒。
- CreateEnv 的 v1.24.4 路径还包含 schema 注册、内部 EP 和 Windows 设备发现；CPU-only 也会走发现流程，尚未对其内部 CPU 栈采样，不能指认其中某一步为根因。EnableTelemetryEvents 实测微秒级。修正 API 下标后的 MaxEfficiency／默认 EP 对照未显示秒级 Session 差异。

源码依据：[延迟创建 Environment 的绑定](https://github.com/pykeio/ort/blob/v2.0.0-rc.13/src/environment.rs)、[ORT 环境初始化](https://github.com/microsoft/onnxruntime/blob/v1.24.4/onnxruntime/core/session/environment.cc)、[Windows 设备发现](https://github.com/microsoft/onnxruntime/blob/v1.24.4/onnxruntime/core/platform/windows/device_discovery.cc)。现有后台预热本来就会触发首次 Session；仅提前显式调用 Environment::current 是移动计时边界，不能称为修好了冷启动。

### 闲置、线程池与系统调度

- 六组每组三对的同进程 10 秒闲置试验，未观察到稳定的“第一轮慢、第二轮快”；第一轮和立即第二轮均有波动。不能把偶发慢直接判为模型过期或重新下载。
- 默认四线程在每次识别后的 10 秒观察窗仍消耗约 3.8–4.0 秒累计 CPU 时间；显式关闭 intra／inter spinning 后约为 0–16ms。关闭自旋没有消除秒级初始化，后续吞吐也未稳定改善，因此未据此修改生产设置。
- 一分钟闲置的补充试验出现一次 1171→304ms 的连续两轮；另一次为 584→571ms。慢的一对中，det／cls／rec 及 Rust 预后处理同时变慢，工作集从 154MB 降至 113MB，但两轮 page fault 为 7677／7438，缺少 hard-fault／I/O 证据，不能把总 page fault 当作换页磁盘读取。该批次有编译负载，未作为无干扰复现或提速结论。
- 只对自己启动的实验子进程请求 HighQoS，两轮为 204／209ms，对应交替默认轮为 425／473ms；预后处理也约缩短一半。继续保持 HighQoS 并闲置一分钟，两对连续调用为 194→219ms、199→211ms，未再出现大幅首次变慢。这提示系统调度／执行速度值得验证，**不能据原始 PowerThrottling control=0/state=0 推断实际 QoS，也不能断言已落到 P 核**。该批次同样有编译重叠、只测一图，不据此承诺产品稳定快一倍。设置只作用于自己的实验进程，未修改系统电源计划或运行中的 RustShot。

当前可确认的是存在秒级新进程初始化与明显的线程池空转；闲置后的额外波动尚未锁定单一原因，真实点击链没有持久 OCR 分段日志。下一步应在无编译重叠时复测 HighQoS 与较长闲置，并在真实请求记录锁等待／引擎复用／资产准备／环境与会话构造／推理各阶段；日志不包含截图或识别文本。不要通过修改系统电源计划、固定 P 核或杀旧宿主掩盖问题。

原始数据在 `target/ocr-experiment/idle-diagnosis`。`native-ep-policy-corrected.json` 才是有效 EP 对照；旧 `native-ep-policy.json` 使用了错误的临时 API 下标，已作废，不作任何性能依据；现有 `Tools/ocr_kernel_native.py` 的固定 API 表未受此临时探针错误影响。报告使用已编译的生产管线调用器与 Win32 子进程 A/B；后来新增的 Environment 独立分段／会话顺序／QoS guard 等 Rust 探针还未编译，受共享编译占用影响，没有将未运行的分支写成实测结果。

## 完整 Rust SIMD 复刻与当前客户端同管线实测（2026-10-15）

**普通 Rust 基准实现约提速 6.4 倍，但相对现有 ORT 没有测出稳定收益。** 本次继续补齐 Tiny 的完整 F32 推理路径，不止两个卷积子图。实验仍全部位于 `target/ocr-experiment`，两个下载原版保持干净，现有 OCR 后端、资产和入口未替换。

### 严格同单线程的结果

同一个 P 核、同一套客户端 Tiny det／cls／rec／字典、相同 BGR 像素，直接复用当前 `pipeline.rs` 的预处理、检测重试、裁行、CTC 与字素证据；各后端只在 ModelRunner 边界切换。三个输入的 det 形状为 `[1,3,736,640]`、`[1,3,1248,1504]`、`[1,3,480,736]`，保持客户端原本 736／3072 策略，不改为参考版的 960。执行完整输出 Softmax，未用紧凑 CTC／ArgMax 捷径。

每图预热一次、测三次。下表单位为 ms／图，统计量是**三张图各自中位耗时的均值**；包括预后处理、推理、张量复制，排除文件读取、解码、模型加载和结果序列化。Rust 为开发优化构建 `opt-level=2`、关闭 debug／overflow 检查；C# 为 Debug 加 `Optimize=true`，均非 Release。

| 对照 | 普通／优化前 | SIMD 复刻后 | 结果 |
| --- | ---: | ---: | --- |
| 普通 Rust 参考执行器 → Rust NHWC SIMD | 2570.74 | 400.75 | 快 6.41 倍，耗时下降 84.41% |
| 当前客户端 ORT → Rust NHWC SIMD | 322.63 | 400.75 | Rust 复刻仍慢 24.21% |
| 原版 C# 同编译图，关闭 → 开启硬件内在函数 | 2553.39 | 394.86 | 快 6.47 倍，耗时下降 84.54% |

C# 接同一条 RustShot 管线，调用未修改原版的公开 `InferenceSession`，另有二进制 IPC，表内保留整个往返开销；SIMD 单线程的 IPC 额外耗时约 19ms／图，不能扣掉它再声称是 Rust 实测。关闭硬件内在函数的子进程握手确认 AVX2／FMA 均不可用，开启时两者可用；保留同样 NHWC 编译图、融合和线程预算。此行是参考方案自身的收益旁证，不是当前 ORT 的性能基线。

另有同 NHWC／预打包／融合、关闭显式 SIMD 的 Rust 消融模式，约 7256ms／图，开启后快约 18.1 倍；这个标量布局比普通 Rust 参考执行器更慢，**不采用其 18.1 倍作为“正常实现”的提速结论**。

### 默认四线程的结果与波动

不限进程亲和性、使用本机全部 32 个逻辑 CPU，保持客户端默认四线程。最终 Rust 复刻的两轮为 **346.16／351.25ms／图**；ORT 三轮为 **198.08／203.21／205.69ms／图**，另有完整一轮为 **583.70ms／图**。检测到编译重叠的一轮整轮弃计后重跑；上述较慢的重跑没有检测到编译，结果仍保留。整轮状态发生漂移，现有证据不能确定具体原因，不能删除较慢轮，也不能平均后反推稳定正收益。

结论限于：**默认四线程未获得稳定提速，多数批次仍慢于现有 ORT**，不汇成单一收益百分比。原版 C# 同管线四线程另测约 230.63ms／图，其中 IPC 约 15.84ms／图，仅供参考。限制在 8 个 P 核时，ORT 三个 session 的结果明显不同；这种受限亲和性的结果不作为当前默认运行收益。

### 实现、验证与范围

- 自有 `rust-simd-kernels/src` 已实现 NHWC／权重预打包、AVX2/FMA 普通 Conv、6 像素×16 输出通道 PW、通道分块 DW、ConvTranspose、MatMul、池化、连续逐元素与完整 Softmax；复用持久线程池，PW 按展平像素切片，覆盖 H=1。Conv／PW／DW 在最终写出前融合 Relu／HardSwish；执行器提前扩展融合目标生命周期，避免覆盖仍需读取的输入。
- `rust-optimized-reference` 复用原版导入器和执行器，保持 NHWC 布局岛；补齐实际客户端 CLS 中 I32／I64 Constant 形状元数据，资产本身未改。`off` 模式走原普通 NCHW 算子，`scalar` 与 `simd` 共用 NHWC／预打包／融合规则。尚不能宣称已经复刻全部 C# 性能细节或达到其四线程性能上限。
- 内核 **37 项**测试通过；修改后的执行器单元与 runtime 集成 **63 项**通过，包含融合目标与输入存储不重叠、TRACE、重复 reshape、尾通道、线程等价、长核激活时机及独立 f64 计算对照。原版冻结的逐位 Erf／偏置抵消测试保留 off 语义；实验 SIMD 明确按原 C# 向量多项式与单输出转置卷积归约顺序验证，不伪称浮点逐位等价。
- 所有已计时条件的三图 **35 行、247 词完整结果 JSON 逐项相同**，含文字、顺序、矩形、几何、方向和字素证据；重复采样稳定，模型调用次数与形状一致。这些输出未保留原始模型张量，不能把几何差 0 写成张量误差 0；也不代表更大语料、Small／Medium 或冷加载已经验证。

最终测量入口为 `target/ocr-experiment/current-app`，原始数据在 `target/ocr-experiment/full-nhwc`。`rust-simd-1t-a.json`／`rust-off-1t-a.json`／`ort-simd-1t-a.json` 是主表数据；`native-rust-simd-4t-final-*.json` 是最后四线程实现，较早的四线程 Rust 文件保留为优化前对照。`csharp-tensor-server` 只供实验调用原版，不成为产品运行依赖。

## 早期方法（保留作历史对照）

- 同一模型：SimdPaddleOCR 仓库内 Tiny `det.onnx`、`rec.onnx` 与我们 `ppocr-cpu.json` 固定的 SHA 完全一致；`cls.onnx` 不同，纯推理对比统一用我们的文件。
- 数据：SimdPaddleOCR 自带固定种子生成器合成的 100 张中英文图（640–1792 长边，带标准答案）。
- C# 用其 `Sdcb.SimdPaddleOCR.Tests --benchmark --workers 4 --model tiny --engine sharp --warmup 1`（.NET 10 Release）。
- Rust 用 `src/ppocr/bench_tests.rs`（默认忽略）：
  `RUSTSHOT_OCR_BENCH_DIR=<目录> cargo test --lib ppocr::bench_tests -- --ignored --nocapture`
  `ocr_bench_dir` 测端到端和 det/cls/rec 推理耗时，`ocr_engine_shapes` 测同形状纯推理。图片预解码，不计解码。
- 机器 i9-14900HX，用户桌面同时在用，绝对值有 2× 波动；结论只取同时段交替运行的比值。

## 结果

准确率（同一评分脚本，去空格精确行 / 近似 CER）：C# 941/1036、1.35%；Rust 935/1036、1.17%，持平。

端到端（4 线程，同时段交替跑，中位 ms/图）：C# 178–194；Rust 修复前约 700+，修复 `contour_score` 后 670–730（高负载时段）。较空闲时段 C# 约 110，Rust 修复前约 410–560。

**同形状纯推理（4 线程，中位 ms）：ORT 不慢于 SimdPaddleOCR。**

| 输入 | ORT 1.24.4 | SimdPaddleOCR |
| --- | ---: | ---: |
| det 736² | 93–101 | 118–127 |
| det 960² | 170–182 | 168–199 |
| det 1312² | 350–374 | 318–392 |
| cls 80×160 | 3.3 | 4.2–4.6 |
| rec 48×320 | 9.3 | 15 |
| rec 48×960 | 24 | 38–40 |
| rec batch 8×48×320 | 60 | 107–115 |

单线程时 SimdPaddleOCR 的 det 736²/960² 明显快（90/162 vs 310/585），1312² 两者持平；我们默认 4 线程，不受益。

## 结论

差距来自我们自己的管线，而不是推理引擎；**现有对比不支持为了速度从零重写推理引擎**。已明确的潜在收益是分发瘦身：Release 包已随附 `onnxruntime.dll`（解压 14.2MB，在发布 ZIP 中约占 5.2MB）和 VC++ 运行库（约 0.9MB），替换 ORT 能省掉相应运行依赖，模型约 7.3MB 仍然需要；74.4MB 的官方 ORT 压缩包仅在运行库未随包（调试版等）且缓存未就绪时下载，5.2MB 并非客户端下载包大小。从零移植约 4.6 万行 C# 算子/解释器并长期维护数值一致性成本较高；已有 Rust 移植降低了试验门槛，其性能仍需独立实测。

## 已有 Rust 实现核查（2026-10-15）

用户要求先查现成复现。本轮仅核查公开源码与上游 CI，没有编译、测速或切换 RustShot 后端。

- **直接移植：[simd-paddle-ocr-rs](https://github.com/mg-chao/simd-paddle-ocr-rs)**。已具备 ONNX 解析／规范化、模型编译、张量存储、Conv（含 1×1／depthwise 分支）／MatMul 等算子，以及 det→cls→rec、CTC 对齐和像素裁行；Apache-2.0，运行依赖仅 serde、sha2，不依赖 ORT。当前 README 明确 tiny／small／medium 使用串行 F32／NCHW 内核，尚无显式 SIMD、多线程和 GPU；不能因名称带 SIMD 就当成原版高性能路径已移植。
- **正确性证据**：当前 main 的 Windows／Linux Rust core CI 通过，默认测试使用合成模型。另一个已通过的 [Scalar OCR validation](https://github.com/mg-chao/simd-paddle-ocr-rs/actions/runs/38663316453) 属于此前开发分支，工作流强制获取真实模型参考资产，比较三种模型的中间节点、识别、完整 OCR 与 CTC 结果；[参考资产说明](https://github.com/mg-chao/simd-paddle-ocr-rs/releases/tag/fixtures-v2) 明确这些证据不证明通用准确率或提速。不能把该分支的验证直接当作当前 main 的本地验收。
- **性能边界**：上游[历史性能报告](https://github.com/mg-chao/simd-paddle-ocr-rs/blob/239664e821ffb4df3bd54746e7722290efa37737/docs/all-model-performance.md) 在固定 160×58 两行小页、单 worker 条件下，自报 Tiny 完整 OCR 标量 Rust 约 71ms、优化 C# 约 27ms；Small／Medium 差距更大。报告针对历史版本和窄语料，本机未复跑，不能据此预测 RustShot 延迟，但已足以说明功能移植完成不等于性能优化完成。
- **其它路线**：[pure-onnx-ocr](https://github.com/siska-tech/pure-onnx-ocr) 用 tract 执行 PP-OCRv6 tiny／small／medium，后处理用 i_overlay；[rusto-rs](https://github.com/byrizki/rusto-rs) 用 RTen。这些是复用通用 Rust 推理引擎的 OCR 管线，不是 SimdPaddleOCR 专用算子的直接移植。前者底层有汇编内核及原生构建步骤，后者默认 `geo-clipper → clipper-sys` 仍含 C++；“不依赖 ORT”与“全链路没有原生构建依赖”需分开判断。

已直接下载两个参考项目并完成原版测试，后续复用现成 Rust 执行器开展独立优化实验。较早的 DW／PW 内核实验保留作局部对照；它没有复现 C# 当前默认的 NHWC 编译图，不能替代原版端到端基线。

## 两个原版与独立优化实测（2026-10-15）

范围严格限定为测试实验：两个下载原版位于 `target/ocr-reference/{SimdPaddleOCR,simd-paddle-ocr-rs}`，测试后 Git 工作区均保持干净；调用程序、样本 BGR 和优化副本均在 `target/ocr-experiment`。不替换 RustShot 现有 OCR，不改其运行依赖、配置或启动入口；所有 Rust 编译共用主工作区 `target`。

原版直接测试：

- Rust：`cargo test --locked --all-features`，86 项单元／集成测试与 1 项文档编译测试全部通过。本机使用已安装的 Rust 1.90.0，未另装上游建议的 1.97.1。
- C#：上游 `KernelCorrectnessTests`、`NhwcKernelTests`、`InferenceSessionRunContractTests` 共 51 项通过；.NET 10，Debug 加全局 `Optimize=true`、`DebugType=portable`，原版核心与调用程序均采用该配置，没有 Release 构建。
- 上游 Rust 是库，没有完整 OCR 命令行，所以独立 caller 只按公开 API 组装原版 det→cls→rec；C# caller 同样直接调用公开 API，避开原版 benchmark 默认开启逐算子 profiler、隐含多线程造成的口径差异。两边原版源码均未改动。

数据用 C# 原版固定种子生成器的前 3 张 JPG（864×976、1488×1248、928×592），只解码一次为共享 BGR，两个引擎读取完全相同的像素与同一份 Tiny 模型／字典。DET 长边上限 960，实际输入分别为 864×960、960×800、928×608；REC 高 48，自适应宽度向上取 32 倍数、不设 320 上限，启用 CLS、关闭 CTC spans。输入读取、解码、模型加载、结果序列化均不计时；每张预热 1 次、普通推理 3 次，另跑 1 次 profile 作诊断。

进程均限制在同一逻辑核（`ProcessorAffinity=1`），Rust 串行；C# 进程局部设 `DOTNET_PROCESSOR_COUNT=1`、`PPOCR_DET_THREADS=1`、行 worker=1。正式计时中 profiler 关闭。用户桌面仍在使用，按 Rust 原版→C# 原版→Rust 优化→Rust 原版复测串行执行，没有同时编译或运行其它性能任务。下表是每张图普通推理的中位数，单位 ms；Rust 原版区间为优化前后两次基线，避免把负载变化全算成优化收益。

| 样本 | C# 原版 1T | Rust 原版 1T，两次基线 | Rust 实验优化 1T |
| --- | ---: | ---: | ---: |
| img-001，11 行 | 471.714 | 2936.278–3147.758 | 2095.836 |
| img-002，15 行 | 645.355 | 2978.555–3024.820 | 2427.311 |
| img-003，8 行 | 273.789 | 1817.063–2098.292 | 1304.231 |

正确性：两个原版共 34 行文字与顺序全部一致、检测框数与缩放尺寸一致；C#／Rust 坐标序列化后的最大差为 `5.08e-5 px`。优化版的全部结果字段（文字、坐标、分类／识别分数、旋转、识别尺寸）与两次 Rust 原版逐项相同，重复运行也相同。这里只验证这 3 张样本，不能据此声称全语料准确率相同。

第一处优化只改独立副本的 `conv1x1` 派发：

- `rust-optimized-reference` 保留原版执行器／模型解析／后处理，新增安全调用，不接受时完整回退原版；未改其 `forbid(unsafe_code)`。
- `rust-simd-kernels` 是独立实验 crate，参考多输出／空间分块结构，使用 AVX2 的 4 输出 × 16 空间块与 8 条累加链。保留原版 bias-first、输入通道升序、分离乘加，不使用 FMA，以便精确核验语义。
- 安全入口先检查分组、溢出与所有切片长度；3 项测试通过，包含 64 组 batch／group／通道尾／空间尾组合，以及抵消、负零、防 FMA 和错误输入不写输出。实际全流程记录到 10,578,800 个向量块，确认不是只命中标量尾部。

三张图中位耗时之和从两轮原版的 7731.896／8270.869ms 降到 5827.377ms，减少 **24.6%–29.5%**。这说明复用现有 Rust 执行器后，小范围 SIMD 改动已经能带来完整 OCR 收益；它仍明显慢于当前 C# 原版。C# 当前主路线是 NHWC 编译图、预打包权重、沿连续通道计算 DW，并可融合激活／残差；本轮只加了 NCHW PW，尚未移植这些主要优化，下一步应沿此结构验证，而不是继续从零补解释器。

关键产物：`target/ocr-experiment/comparison.json` 汇总同批对照；`rust-baseline-p1.json`／`rust-baseline-repeat-p1.json` 为原版，`rust-optimized-p1.json` 为优化版，`csharp-baseline/tiny-1t-p1.json` 为 C# 原版；实验代码在同目录的 `rust-baseline`、`rust-optimized`、`rust-optimized-reference`、`rust-simd-kernels`、`csharp-baseline`。没有改正式应用代码，也没有验证 Small／Medium、全 100 图、多线程、冷启动或内存收益。

主要命令（在项目根目录运行；复测性能时仍需为启动进程设置上述亲和性／线程参数）：

```powershell
$env:CARGO_TARGET_DIR = (Resolve-Path target).Path
cargo test --locked --all-features --manifest-path target/ocr-reference/simd-paddle-ocr-rs/Cargo.toml
dotnet test target/ocr-reference/SimdPaddleOCR/test/Sdcb.SimdPaddleOCR.UnitTests -c Debug -p:Optimize=true -p:DebugType=portable --filter "FullyQualifiedName~KernelCorrectnessTests|FullyQualifiedName~NhwcKernelTests|FullyQualifiedName~InferenceSessionRunContractTests"
cargo build --offline --manifest-path target/ocr-experiment/rust-baseline/Cargo.toml
cargo test --offline --manifest-path target/ocr-experiment/rust-simd-kernels/Cargo.toml -- --test-threads=1
cargo build --offline --manifest-path target/ocr-experiment/rust-optimized/Cargo.toml
target/debug/ocr-reference-rust-baseline.exe target/ocr-experiment/inputs 3
target/debug/ocr-reference-rust-optimized.exe target/ocr-experiment/inputs 3
```

## 较早的独立卷积内核实验（2026-10-15）

用户确认参考已有实现后开发自己的版本。本阶段先独立实现可对照的卷积内核，不移植完整 ONNX 解释器；`src/ppocr/kernels.rs` 与 `kernel_probe_tests.rs` 均由 `cfg(test)` 接入，不进入正常 OCR 后端，无新运行依赖。

- NCHW／OIHW F32：depthwise 3×3、5×5（高宽独立步长 1／2、非对称补边、batch）和无补边的 pointwise 1×1。支持标量、自动派发、强制 AVX2；自动路径同时检测 AVX2 与 FMA，缺少指令集回退标量。DW 对固定核与步长分别特化，按 8 个横向像素计算，边缘／尾部用带掩码的 gather／store；PW 用 4 输出通道 × 16 像素、8 条累加链复用加载与广播。
- 入口先校验维度、乘加溢出、精确缓冲区长度与输出预算，再写输出；不支持的卷积明确报错。`kernels_tests.rs` 以独立 f64 输入散射卷积为参考，覆盖步长、补边、小宽度、通道尾、批量与非法输入不写输出，新增 312 组跨 SIMD 边界的形状／补边组合。
- `Tools/prepare-ocr-kernel-probe.py` 仅从指定本地 ONNX 导出直接相连的 depthwise→pointwise、对应权重及前缀模型；不跳过激活／Add，不下载数据。Tiny det 736² 有三种代表组：3×3 s1（DW 输入 32×184×184）、3×3 s2（同输入）、5×5 s1（64×23×23）。Tiny rec 的 DW 后有其它算子，本轮不造不真实的双 Conv 子图。
- 对照入口先用 ORT 前缀从固定随机／零／渐变源张量生成真实中间激活，逐层与串联分别验证标量、自动 SIMD、ORT 1／4 线程。参考子图禁用图优化，计时子图使用 Level3；容差 `1e-5 + 1e-4 × |reference|`，同时拒绝非有限输出并报告最大绝对误差／RMSE。
- 首版开发构建的 DW 边缘标量循环、PW 分块与内层索引开销较大；汇编还确认开发配置的 debug／overflow 检查留在 SIMD 热路径。仅关闭检查改善有限，主要改进来自上述内核重写。首版“比 ORT 单线程慢 2.7–9.6 倍”的结果已由下表替代，不能用来判断 Rust 性能上限。
- 最终计时由 `Tools/compare-ocr-kernels.py` 通过开发用 `ocr-kernel-ffi.rs` 和 `ocr_kernel_native.py` 在同一进程加载自有 Rust DLL 与客户端实际 ORT DLL。两者均复用输入、DW 中间输出与 PW 最终输出；ORT Level3、顺序执行、inter=1、intra=1／4、关闭自旋，使用预创建的 OrtValue 调用 Run。轮转 18 轮、去掉前 3 轮，取余下 15 轮中位数，排除导出、加载、前缀、校验与输入复制，保留各自 API 调用开销。
- Python 的同版本 ORT 轮子内嵌另一套构建，性能与客户端 DLL 明显不同，因此默认只作数值参考；不能拿轮子的耗时替代客户端基线。以下仍是两个卷积的热调用比较，未验证完整 OCR 延迟、冷加载或内存。

复现（需要开发用 Python 的 onnx、numpy，以及已缓存的 Tiny 资产）：

```powershell
python Tools/prepare-ocr-kernel-probe.py --model "$env:LOCALAPPDATA/RustShot/ocr/cpu-api24-tiny-1/ppocrv6-tiny-pinned-1/det.onnx" --output target/ocr-kernel-probe/det --input-shape 1,3,736,736
cargo check --tests
cargo test --lib ppocr::kernels
$env:RUSTSHOT_OCR_KERNEL_DIR = 'target/ocr-kernel-probe/det'
cargo test --lib ppocr::kernel_probe_tests -- --ignored --nocapture --test-threads=1
```

首版的 `cargo check --tests`、10 项单元测试和真实权重 ignored 对照已通过。优化后直接编译同一份内核源码，11 项单元测试全部通过（另 1 项测速默认忽略）；同进程对照的 3 组子图 × 随机／零／渐变前缀激活也全部通过，最大绝对误差 `4.77e-6`，所有元素满足上述逐元素容差。

无需重建整个 GPUI 应用即可复核当前内核；仍在主工作区 `target/debug` 产出，直接引用原源码，不复制工程、不构建 Release。开发用 Python 另需 onnxruntime 1.24.4（下例局部安装，不进入客户端依赖）：

```powershell
rustc --edition 2021 --test -C opt-level=2 -C debug-assertions=no -C overflow-checks=no src/ppocr/kernels.rs -o target/debug/ocr-kernels-verify-fast.exe
target/debug/ocr-kernels-verify-fast.exe
rustc --edition 2021 --crate-type cdylib -C opt-level=2 -C debug-assertions=no -C overflow-checks=no Tools/ocr-kernel-ffi.rs -o target/debug/ocr-kernels-tuned.dll
python -m pip install --target target/ocr-kernel-python --no-deps onnxruntime==1.24.4
python Tools/compare-ocr-kernels.py --kernel-dll target/debug/ocr-kernels-tuned.dll --cpu-mask 0x55
```

本机 i9-14900HX，基准进程限定 4 个 P 核各一逻辑核（`0x55`），Rust 为单线程 AVX2/FMA、`opt-level=2` 并关闭开发检查。客户端为缓存中的 ORT CPU 1.24.4 API24。桌面持续使用，下表只比较同进程同批轮转测量，单位 ms；不与前次不同负载／亲和性的绝对耗时相减。

| 真实双卷积子图 | 自有 Rust 1T | 客户端 ORT 1T | 客户端 ORT 4T | Rust DW | Rust PW |
| --- | ---: | ---: | ---: | ---: | ---: |
| 3×3 s1 → 1×1，32×184×184 输入 | 4.9889 | 5.8830 | 3.0735 | 1.0440 | 3.9188 |
| 3×3 s2 → 1×1，32×184×184 输入 | 1.5463 | 1.8233 | 1.0272 | 0.8201 | 0.5889 |
| 5×5 s1 → 1×1，64×23×23 输入 | 0.2903 | 0.1339 | 0.2502 | 0.2658 | 0.0247 |

结论：两个大图子图已与 ORT 单线程处在同一水平，本次约少用 15% 时间；仍慢于 ORT 4 线程。小图 5×5 仍慢约 2.2 倍，主要耗时在 DW：23 像素宽下多数向量块要处理边缘 gather，25 个 tap 的累加和寄存器压力仍有改进空间。DW／PW 独立中位数来自不同测量轮次，不要求与串联中位数精确相加。

该独立内核在真实权重上接近成熟单线程内核，性能取决于实现与布局；现有结果仍不足以替换正式后端。此模块自身没有推理执行器；上节的完整 OCR 实验已改为复用参考版执行器。正式后端接入、更多模型／语料、冷加载和内存仍未验证。

Rust 端到端拆分（较空闲时段均值，ms/图）：BGRA→f32 RGB 29；检测阶段 320（其中 det 推理 70、`contour_score` 约 250）；行识别 112（cls 25 + rec 52 推理，其余裁行等约 35）；墨迹收紧 4。

## 已做

- `pipeline.rs::contour_score`：原来对外接框内每个像素遍历整圈轮廓做点在多边形判定（像素×顶点）。改为先把边按行分桶，每像素只查本行接触的边；奇偶与边界判定逐位等价。该项 349–504ms → 11–12ms（同负载），100 张输出文字逐字节相同；`cargo test --lib ppocr` 除既有环境/并行改动导致的 17 项外全部通过。

## 后续可做（按收益排序，均未实施）

1. **det 输入尺寸**：长边 ≥1280 时 `max_side=3072`，大图按原分辨率推理（1312² 约 350ms）；C# 统一上限 960。需用真实高 DPI 截图评估小字准确率后再定上限，不要只按合成集调。
2. **行级并行**：rec/cls 现为逐行 batch=1 串行。C# 用 4 个单线程 session 并行行；ORT rec batch 8 也比 8 次单张省约 20%。可按宽度分桶 batch 或多 session 并行。
3. **预处理**：`RgbImage::from_bgra` 转 f32 三通道约 29ms，det 预处理逐像素 f32 双线性插值并逐行 checkpoint 约 35–80ms；改为 u8 直接缩放写张量。
4. 裁行杂项约 35ms，待细分。

## 补充：单线程为何更快、大小核影响（2026-10-13）

本机 i9-14900HX 为 8P+16E 混合架构。不限亲和性时多线程会落到 E 核，fork-join 等最慢线程；单线程常在 P 核。用 `ProcessorAffinity=0x5555`（8 个 P 核各一逻辑核）重测同形状纯推理（ms）：

| 输入 | ORT 4T | SIMD 4T | ORT 1T | SIMD 1T |
| --- | ---: | ---: | ---: | ---: |
| det 736² | 64 | 59 | 174 | 99 |
| det 960² | 106 | 97 | 296 | 155 |
| det 1312² | 202 | 198 | 549 | 613 |
| rec 48×320 | 5.8 | 9.8 | 13.5 | 16 |
| rec 8×48×320 | 35 | 62 | 112 | 124 |

- 4 线程延迟两者持平，rec ORT 快约 1.7×；SIMD 只在单线程 det（≤960²）每核效率高约 1.8×，且其 2 线程几乎不比 1 线程快。重写推理引擎换不来延迟收益，结论不变。
- 同样限 P 核时 SimdPaddleOCR 端到端 135ms，我们反而升到约 875ms（det 均值 213、rec 242，远高于同形状单测）。怀疑 det/cls/rec 三个 session 各自 4 线程池且 ORT 默认自旋，12 个自旋线程挤 8 核互相抢占；可试共享全局线程池或 `session.intra_op.allow_spinning=0`，并用 `SetThreadInformation` 关闭 OCR 线程的功耗节流以偏向 P 核。尚未实施，真实前台截图场景的调度也未验。

## 固定下载来源与缓存（2026-10-27）

当前 Tiny／Small 清单使用 Bitiful 镜像与固定官方来源。文本识别模型的备用源为 PaddlePaddle 的 Hugging Face 固定提交 URL，字典与许可证的备用源为 PaddleOCR 的 GitHub 固定提交 URL；模型、字典与许可证的大小和 SHA-256 保持不变，已有合格缓存可继续复用。表格许可证共用相同双源，结构模型仍使用已核验的 ModelScope 固定版本。

下载地址随程序固定在 [Tiny 清单](../../dependencies/ppocr-cpu.json) 与 [Small 清单](../../dependencies/ppocr-small-cpu.json)，选择 Small 后自动准备所需文件，无需手填下载地址。已有 Tiny 缓存时，Small 额外下载 det／rec／字典共 31,114,837 字节（约 29.7 MiB）。2026-10-23 再次直接从清单地址完整下载这三个文件，大小与 SHA-256 全部匹配；共享方向模型、许可证和 ORT 官方下载入口均返回 HTTP 200。

运行库优先使用 Bitiful 的 5,294,996 字节小包，备用源为 Microsoft 官方 `onnxruntime-win-x64-1.24.4.zip`，固定 74,442,783 字节；两包分别校验自身大小和 SHA-256。两者只解出同一组 `onnxruntime.dll`、`onnxruntime_providers_shared.dll`、许可证与第三方通知四个成员，并逐项校验；不解出调试符号。运行库成员就绪后删除本次压缩包和旧版遗留的官方整包。Release CI 同样保留镜像失败后的官方源回退。

Release 便携包继续只随附所需的四个运行库成员；其中 `onnxruntime.dll` 在发布 ZIP 中约占 5.2MB。运行库缺失时才下载压缩包。多来源并行读取最多 256 KiB，探测上限 3 秒，按预计完整下载时间排序；连接、正文或大小／摘要校验失败后尝试下一来源。

2026-10-27 针对 `det.onnx ConnectionFailed` 反馈，使用宁波与海外节点绕过代理、完整下载正文并流式核验大小和 SHA-256：8 项唯一模型／字典／许可证镜像及运行库小包，两节点共 18 次均为 HTTP 200 且身份匹配。宁波 Hugging Face 检测模型连接失败，海外同地址通过，说明只保留官方单源会阻断部分国内网络。海外另完整核验全部 9 项官方备用来源，均匹配；ModelScope 表格模型在国内／海外分别约 1.68／2.97 秒，均匹配。本机也完整核验全部镜像及运行库小包中的四个成员。以上只代表这次节点样本，不覆盖所有运营商。

本轮 `cargo check --lib` 与 79 项资产相关测试通过，覆盖双源清单身份、不可达来源跳过、校验失败换源及缓存／取消；旧取消测试首次缺少专用暂存目录配置，补齐配置后该模块通过。Release 下载脚本的 PowerShell 语法检查通过。

0.4.16 的 CI 因账户付款或支出限额未启动；用户授权临时检出与本机 Release 后，从干净发布代码复用主工作区 target 构建并发布。新版 CLI 在独立空数据目录依次完成检测、Tiny 全文与 Small 首次识别，两行合成样本均正确识别；约 4.63／5.45／8.64 秒仅为本次样本，不作网络或识别耗时承诺。新程序包在宁波／海外完整匿名下载约 5.70／7.57 秒，大小及摘要全部匹配。严格 Clippy 仍被既有全库告警阻断；本次没有实际截图 GUI 验收。

## 快速换源（未发布）

0.4.16 先并行做短段探测，再正式下载；但探测的请求时限不能覆盖系统 DNS 阻塞，正式请求还沿用较长的等待和同源重试。ureq 的整体请求时限又会覆盖 socket 读写空闲时限，因此不能仅凭原来的读超时配置保证卡顿后快速换源。

后续源码将 OCR 资产与其它后台 HTTP 的等待策略分开：DNS 最多等 3 秒，连接预算 3 秒，读写连续空闲 5 秒就结束当前来源；多源失败后直接尝试下一个来源，不先在原来源重试。单源仍保留立即断线等短暂故障的有限重试，超时不自动重试。短段探测继续并行，失败来源继续在本进程内降低优先级。

正常持续有数据的慢下载允许超过 5 秒；正文读取层保留原有 10 分钟预算，文件大小、SHA256 与原子落盘保持不变。资产请求禁用空闲连接池，避免连接回池后清掉 socket 超时、正式下载又复用无空闲预算的连接。系统 DNS 超时后只允许最多四个未完成的纯解析线程继续收尾，它们不会继续发起 HTTP。

该策略尚未随新版分发，0.4.16 安装包不会自动获得这些调整。

主工作区 `cargo check --lib`、83 项资产测试及 7 项选源／DNS 测试通过。真实 loopback HTTP 覆盖 probe 成功后正式请求不回响应头、正文中途停滞、keep-alive 连接复用，以及持续约 450ms 的正文能够跨过 100ms 空闲预算完成；停滞用例均确认直接换源、无同源重复请求、临时文件清理。DNS 注入用例验证短等待、错误脱敏和四个未完成解析线程的上限。本轮没有构建或发布新的 Release。

## 下载优化历史试验（2026-10-13／19）

以下结果属于当时配置，不代表当前下载策略：

- 10-13 曾从官方包裁出上述四个成员（路径、时间戳不变，deflate 9），生成 5,294,996 字节的小包，可重复生成；空数据目录下载与识别通过，缓存只剩 9 个必需文件，旧整包清理通过。该小包在本轮重新完整核验后恢复使用。
- 10-19 曾对独立镜像与官方源做首段测速，镜像 256KiB 约 2.4s 返回 206，HuggingFace 5s 内未收到字节。这只反映当时的本机网络，不能推断当前或其它网络下的可达性。
- 当时 `ppocr::assets`／`download_source` 82 项测试通过，覆盖来源排序、不可达跳过与校验失败换源；这些历史结果不代替当前来源的下载验证。

## Tiny／Small 配置与交接（2026-10-23）

设置 ▸ 截图 ▸ 文字识别的模型选项明确显示“Tiny · 轻量”“Small · 标准”，保存后提示下一次识别生效。默认仍为 Tiny；识别不理想时可尝试 Small。截图、贴图、图片编辑、表格与智能马赛克复用该配置，CLI 仍需显式传 `--ocr-profile small`。

每批请求固定模型身份，局部行重识别沿用产生原结果的模型。控件文字为空转交标准 OCR 时也携带 UIA 链首捕获的 profile，避免查询期间切换设置后改用另一模型；普通 OCR 的读取时机保持不变。Small 的 det／rec／字典独立校验与缓存，只共享固定身份相同的运行库、方向模型和许可证，失败不回退 Tiny。

本机已有 Tiny 缓存、尚无 Small 缓存时，用现有 Dev CLI 对自制 1000×240 两行中英文图片执行 Small 识别，完成真实下载、校验与 ORT 推理；再用省略模型参数的 Tiny 识别同图，两者均完整得到“中文识别测试 2026”和“Small OCR model 12345”。这只确认模型可用与默认档位，不代表两者准确率高低或性能差异；真实设置窗口与复杂图片另待验证。

本次 `cargo check --bin rabshot-gpui` 通过；`cargo test --lib profile` 78 项通过、2 项原生媒体专项保持忽略，包含配置兼容、模型／字典隔离及 UIA 空值交接后保留 Small 的回归。
