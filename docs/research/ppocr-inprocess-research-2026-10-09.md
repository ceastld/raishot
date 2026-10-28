# OCR 参考工程 Rust 进程内重写：研究与实施记录（2026-10-09）

## 2026-10-09 Bitiful 独立下载已实施，Release 文件交付与首下载通过

用户新增约束：运行依赖由 RustShot 自己后台 HTTP 下载并验证固定 bytes／SHA，缓存 `%LOCALAPPDATA%/RustShot/ocr`。Quicker 仅为可选启动／本体分发入口；运行依赖不得使用 Quicker CEA、dependencycheck、MCP／RPC、宿主目录或 secret，不把 Workspace、.NET 或参考实现变为运行依赖。

用户指定自己的 Bitiful Cloud 优先托管固定运行依赖，官方来源继续保留 provenance。发布端凭据 `BITIFUL_*` 只放维护者仓库外的 `.env` 或 CI Secret；这些凭据仅用于发布，不进入客户端、运行清单或便携包。本记录不包含任何密钥。

Bitiful 实际发布已完成：`target/qa/bitiful-ocr-publish-20261010-050033-159/receipt.json` 自然退出 0，运行库 ZIP、det、rec、词典和许可五件均单次 Put 完成、`anonymous_verified=true`，`all_public_readbacks_verified=true`。默认公网基址为 `https://s3.bitiful.net/quicker-pkgs/`，固定前缀 `rustshot/dependencies/cpu-api24-tiny-1`，每件完整匿名读回 bytes／SHA 已核，不是只验证上传返回。

生产 `dependencies/ppocr-cpu.json` 五个下载 URL 已切至镜像，并用 `upstream_url` 保留各官方固定来源；该清单为 4627 bytes／`4B7EAC65D1030D35977A07F5FA82073049BD56D9F8D8062C54C21EE06B365302`，资产本身 bytes／SHA、缓存 revision 和 ABI 不变。发布工具 `Tools/publish-ocr-bitiful.py` 只在发布机器使用 Python／boto3，独立 dry-run 回执为 `target/qa/bitiful-ocr-publish-dryrun-20261010-050014-577/receipt.json`；EXE 不依赖 Python／SDK，也不分发发布凭据。

首轮 `0BBF` 的 strict／6 资产模型虽 child exit 0，但同目录三件多屏 `auto_target_rect` 来源变化导致 sourceguard=false，不能记整轮通过。后续实际回执来源 **3852937F／604** 的 strict 及 13 项（6 资产＋7 startup preview）自然 0／来源未变：`target/qa/startup-standby-robust-checks/20261010-050646-152-clippy-9f16cd/verification.json`、`target/qa/functional-iteration-models-20261010-050646-259-cead1b/verification.json`；仅属于该阶段，不扩大到最终来源。

后审发现多屏父目标先降级会丢 retained 资格并将透明预览延后 150ms，现仅修 `capture_control_selection`：先用原始范围求 metadata，再映射显示，主动 navigated 行为保持。最终 **EE7946967E8EEA8E31D169BC80197FCA352919D3D81B8E6030736034F0CBCFEF／604** 的格式、strict 及 7 retention 模型已实际自然退出 0／来源未变，回归直接调用生产 helper 覆盖既有 32ms 动画和无路径变化：`target/qa/startup-standby-robust-checks/20261010-051156-708-formatcheck-c3fbb5/verification.json`、`target/qa/startup-standby-robust-checks/20261010-051156-740-clippy-376856/verification.json`、`target/qa/functional-iteration-models-20261010-051156-822-51a71a/verification.json`。前阶段 13 项与本阶段 7 项分别保留资格，不称单批同源 20 项；前一 `35c32f` 仅由 Root 停止自己创建的 Cargo／rustc，非自然通过，原回执保留，未操作用户程序。

最终共同 Release `87fbc4` 实际 `ok=true`、child exit 0、来源前后 EE／604：`target/qa/startup-standby-robust-checks/20261010-051213-634-release-87fbc4/verification.json`，构建窗口为 05:12:13.94–05:27:10.86 UTC。本机 files-only 交付已完成：`target/qa/native-preview-arrow-file-install-20261010-052808-372-f237de7e15104d81a318c37b9d7318bc/delivery.json`，`ok=true`、`file_install_complete=true`、`runtime_operated=false`、`runtime_verified=false`、`recovery=[]`；旧 1EA2／EDD2／8F01 各双份备份，已安装三件与同源 Release 一致：

| 文件 | bytes | SHA256 |
| --- | ---: | --- |
| rustshot.exe | 623104 | `4B02759B4020379651E74AB6DF1672F1C0D28A6FBF5FD48067231984D0997B99` |
| rustshot-gpui.exe | 17545216 | `8329E748205C2072E3F57A1E8A0335301AAD7ECEAA7AFCE7A6491079719A6012` |
| rustshot-cli.exe | 4025856 | `C2DA12C73E814F014E809E1613FD7A8BCD14A7D634BC0D0E3253886A25EB19A0` |

新版 Debug `6DFF7892` 的空缓存三阶段先已通过：`target/qa/qk-ocr-bitiful-first-download-20261010-052307-454/verification.json`。同一 Release CLI 的前两次网络失败保留：`target/qa/qk-ocr-bitiful-first-download-20261010-052412-731/verification.json` 为 det exit 0、rec 下载 Io／exit 1；`target/qa/qk-ocr-bitiful-first-download-20261010-052724-972/verification.json` 为 runtime 下载 ConnectionFailed／exit 1。连接检查后使用同一已安装 CLI 重跑完整流程，未改源码或降低断言。

最终已安装 `target/release/rustshot-cli.exe` 的完整空缓存验证实际 PASS：`target/qa/qk-ocr-bitiful-first-download-20261010-053157-892/verification.json`。嵌入清单与固定 `4B7EAC65` 完全相同，cache initially absent，det-only 后 rec／词典仍不存在，五件云下载文件的实际 bytes／SHA 均正确；det、补 rec／词典的全文识别、cached self-test 三阶段 child exit 0，中英彩色透明三行及有效词框通过，不使用前台、桌面或剪贴板。单次 cold det 27.54s 包含约 75MB 原始 runtime ZIP 下载，rec 补载加识别 11.08s、自测 5.20s，仅为样本，不作截图约 200ms 启动承诺。

运行实例／真实 GUI／provider／DPI、异机、产品加载和约 200ms 仍未验，不要求桌面空闲。Quicker 远端包／动作本轮未更新，保留并行分发工作；分类、重试、版面、表格／抠图、Small profile、原生取消等仍待办。后续可准备仅必要 runtime 文件的固定小包以减少首次下载，尚未实施，不扩大本轮范围。

## 已交付的官网直下首阶段：普通 OCR 与独立文字检测

用户已明确要求立即实施 Rust 重写。本阶段源码已接入普通 OCR 和 det-only：`src/ppocr/` 用 Rust 实现预处理、检测后处理、透视裁行及 CTC 解码，同进程直接调用固定 ONNX Runtime 1.24.4／API24 原生 CPU 库，复用 PP-OCRv6 Tiny det／rec 与匹配词典，不引入 C#、.NET 或 参考工程 Agent。普通识别保留原 `RecognizedText` 视觉行、词框、全文和 ROI 局部物理坐标合同；独立检测只返回合法框，不伪造文字或词行。截图、贴图的共享 OCR 入口默认 ppocr，Windows OCR 保留为显式独立提供者，失败不静默混合两者结果。

`dependencies/ppocr-cpu.json` 固定运行库、模型、词典与许可的来源、bytes 和 SHA；首次 OCR 后台任务按需下载并校验，活动进程内串行复用 CPU session。检测只准备 det，首次全文识别才补 rec／词典；托盘、待命工作进程和冻结首屏不初始化这些依赖。VC 运行库缺失时只返回明确的官方 x64 安装提示，不自行安装。持续智能候选现接独立检测，保留冻结 ROI、来源身份、180ms 刷新与迟到结果拒绝；贴图／图片的一次性智能效果仍走全文识别，本阶段未迁移到 det-only。

自动 180° 分类、高级稀疏／长图重试、完整 original／auto 版面重排、表格、抠图、Small profile 和原生推理取消尚未实现；本阶段不代表全部 ppocr 功能完成，也不承诺冷加载、准确率、内存或延迟收益。

输入等价复核发现，参考工程／固定 RapidOcr 实现按 B/G/R 填写张量，Rust 原先将内部 RGB 直接写入检测／识别张量，不是等价输入。现已仅修两处输入构造，内部 RGB 和正确的 `mean/std=127.5` 保持。修正后源码冻结为 `ECFA05C1E7981093BF9A66734269FFC678FAB3810F3783E6457A29151AF85151`／603；统一 18 项、格式与 strict、Debug／Release CLI 的真实彩色推理和同源三件套 Release 均已通过，本机仅文件交付完成。

- 修正后同源 ppocr 14＋Recognition 3＋检测候选 1 共 18 项实际自然退出 0，来源前后均 ECFA／603：`target/qa/functional-iteration-models-20261010-043022-754-0bc892/verification.json`。ppocr 模型含 det／rec 彩色输入通道断言，检测候选仍验证冻结 ROI 与局部坐标映射。
- 修正后 ECFA／603 格式和全目标 strict 实际自然退出 0、日志排空、来源未变：`target/qa/startup-standby-robust-checks/20261010-042341-847-formatcheck-de7929/verification.json`、`target/qa/startup-standby-robust-checks/20261010-043039-591-clippy-7383c0/verification.json`。
- Debug CLI `11FF28F9B9D832EA1F2A32A2FCAA7E3DE6D4A11DE24C0861FDEE600CC523A78D` 的彩色透明三行图片检测、全文识别及原中英透明自测三阶段实际 PASS／exit 0：`target/qa/qk-ocr-color-inference-20261010-042910-460/verification.json`。全文为 `RustShot 2026 OCR Test\n中文测试 2026\n第二行文字12345`，words=15、lines=3、regions=15，词框合法；原透明两行自测仍 words=10、lines=2。使用生成夹具和后台 CLI，不读取桌面或剪贴板，不使用前台；Debug 结果仅对应该已测二进制，Release CLI 另见下述独立回执。
- 修正后首次 `461c59` 因工具链环境缺失编译 101、0 模型，第二次 `ad2759` 已通过 ppocr 14＋Recognition 3，但检测候选筛选误写为文件名模块，0 测试导致整批 `ok=false`。两份原回执分别保留于 `target/qa/functional-iteration-models-20261010-042341-838-461c59/verification.json` 与 `target/qa/functional-iteration-models-20261010-042419-321-ad2759/verification.json`；最终 `0bc892` 修正实际注册名称、未减测试并完整重跑 18 项。
- Release CLI `8F010A31DD8D6E784A6F57A76B96AB5FBCBD163C433A7D633CD5194B1D898B38` 也已独立完成相同彩色 det／rec 与原中英透明自测三阶段，实际 PASS／exit 0：`target/qa/qk-ocr-color-inference-20261010-043341-848/verification.json`。全文与 Debug 相同，words=15、lines=3、regions=15。该实测发生于三件套编译结束前，最终 Release 及安装读回的 CLI 字节身份与此实测完全一致，整套完成另以 Release 和交付回执为证。

下面的 `8C6D` 结果仅为 BGR 修正前阶段证据，黑白文字自测通过不代表彩色输入已经对齐。

CLI 已提供纯文件输入和 JSON 输出：`--ocr-file image.png` 默认 ppocr，`--ocr-provider windows` 明确选择 WinRT；`--detect-text-file image.png` 仅运行 ppocr 检测；`--ocr-self-test-ppocr` 使用原 RustShot 2026 夹具及中文、透明背景、多视觉行夹具，要求实际文字及有效词／行框。BGR 修正前完整来源为 `8C6D5CDDE4B102446684F92CD2AB4B83CBCED0FA092C3383E5FCC7B8D8AF0954`／603，该阶段取得以下实际结果：

- 修正前阶段同源格式和全目标 strict 实际自然退出 0、来源未变：`target/qa/startup-standby-robust-checks/20261010-041312-468-formatcheck-5274a5/verification.json`、`target/qa/startup-standby-robust-checks/20261010-040830-340-clippy-95ce49/verification.json`。
- 修正前阶段 ppocr 14＋CLI 3 共 17 项同源模型实际 PASS：`target/qa/functional-iteration-models-20261010-040928-242-22c053/verification.json`。初 54 项整批回执因 CLI 测试模块移至生产 items 后发生来源变化，仍为失败；其已执行的 Recognition 3、检测候选 1、Hybrid 17、Pin 19 共 40 项仅按未变调用链限定保留，资格记录为 `target/qa/qk-ocr-rust-implementation-20261009/retained-40-background-model-qualification-20261010-041412-734.json`。这是 17 同源补验＋40 限定保留，不能称为单批同源 57 项。部分 Pin 文本模型使用内存 GDI 字体上下文，不称 40 项全部无 GDI。
- 真实空缓存、环境代理路线的运行库／模型下载、DLL 加载、det-only 和全文识别实际 PASS：`target/qa/qk-ocr-first-download-20261010-041243-080/verification.json`。检测后 rec／词典仍不存在，全文请求才补载；ppocr 两张图自测得到 `RustShot 2026` 和 `中文测试 2026\n第二行文字`，透明多行结果 words=10、lines=2、regions=10，几何断言通过；显式 WinRT 自测也独立通过。本机单次样本为 det 首次下载／加载／检测全程 11.15s、rec 补载加识别 5.93s、ppocr 两图自测 2.11s，不是稳定延迟、冷加载收益或异机承诺。
- 同目录 `windows-manual-verification.json` 子进程移除全部代理环境变量，以独立空模型缓存走 Windows 手动代理／OS 证书，真实 det／许可网络下载成功；运行库从前述已验证缓存复制，因此这一路没有证明运行库首次网络下载。当前下载支持环境 HTTP 代理优先及 Windows 手动代理／系统证书；PAC、SOCKS 和自动 VC 安装不支持，保留明确错误提示。

修正前 Release `61f59b` 已实际自然退出 0，Cargo 构建 15m39s，但前后来源从 `8C6D` 变为 `ECFA`，`source_unchanged=false`、`ok=false`，未交付；原回执保留于 `target/qa/startup-standby-robust-checks/20261010-041056-023-release-61f59b/verification.json`。

最终共同 Release `5e3a35` 实际 `ok=true`、自然退出 0、日志排空，来源前后均 ECFA／603：`target/qa/startup-standby-robust-checks/20261010-042341-806-release-5e3a35/verification.json`。stderr 为 Finished 16m29s，包含前一构建的文件锁等待，这是构建时长，不是程序启动时长。最终 CLI 与上述 Release 彩色实测身份完全一致。

本机 files-only 交付已实际 `ok=true`、`file_install_complete=true`：`target/qa/native-preview-arrow-file-install-20261010-044110-185-0cf0c66547f747f8a70a58036324a898/delivery.json`。阶段为 `installed-files-runtime-untouched`，`runtime_operated=false`、`runtime_verified=false`、`recovery=[]`；旧 27D58DEF／13600DF9／8E856021 各双份备份已核验，当前 `target/release` 读回为：

| 文件 | bytes | SHA256 |
| --- | ---: | --- |
| rustshot.exe | 623104 | `1EA2B80EAD5853E0E9DC18F178B98F14E05514E8BDF6B49A6E5E05B86811355A` |
| rustshot-gpui.exe | 17539072 | `EDD2836C0D39A3E4037AB4C1D2C2E245C74BF1FD1FAC2F576248DEFEE88BF74C` |
| rustshot-cli.exe | 4025856 | `8F010A31DD8D6E784A6F57A76B96AB5FBCBD163C433A7D633CD5194B1D898B38` |

运行实例未操作，同目录 palette 源码和候选、四件并行分发改动保留；本轮未上传／更新远端 0.4.3，不将动作／打包／上传纳入本轮验证。异机首次下载、真实截图／贴图 GUI、DPI／IME／UIA provider、产品加载和完整约 200ms 延迟仍待自然使用验证，不要求桌面空闲。后台推理通过不代表全部 ppocr 功能或准确率已验，后续分类、重试、版面、表格／抠图等按计划继续。

## 实施前研究记录

用户于 2026-10-09 进一步确认重写方向：以 OCR 参考工程源码和测试数据为行为参考，用 Rust 重写预处理、检测后处理、裁行、CTC 解码、排版及后续表格／抠图管线；复用 ONNX 模型、词典和官方 ONNX Runtime 原生推理库。运行时由 Rust 直接调用推理库，C# Core／Agent／CLI 只作参考，不引入 .NET 运行依赖。这是算法与业务层重写，推理引擎和模型继续复用；以下为当时尚未实施的研究结论，当前范围以上文为准。

用户要求：研究之前 OCR 参考工程的相关功能能否集成进 Rust 进程，除非必要尽量避免进程通信；本次先研究。结论是可行，推荐 Rust 直接调用 ONNX Runtime，在活动截图／贴图进程的后台线程运行。复用模型、字典和原生推理库，移植现有 C# 算法；不把 .NET Agent 作为默认运行依赖。

本次架构建议明确为**默认进程内，IPC 按实测需求再引入**。普通 OCR、文字检测、表格和抠图都没有发现算法上必须跨进程的要求；现有 Agent 的作用是跨截图缓存模型和隔离原生故障。首阶段先做普通 OCR 与独立文字检测，并保留现有 WinRT 路线作为独立提供者；后续再做表格和抠图。进程内方案能直接消费内存图像，但冷加载、准确率和内存收益必须实际测量，不承诺仅移除 IPC 就会更快。

分发继续遵循独立便携软件约定：软件本体、固定版本推理库、模型／词表分开管理，按所选功能下载并校验版本、字节数和 SHA；同一份依赖可用于 Quicker V1／V2，运行时不依赖参考实现或其 Agent。下载完成与实际加载／识别成功分开记录。截图先显示冻结选区，再按需初始化 OCR，模型下载或冷加载均不得成为约200ms首屏的前置条件。

ppocr 研究阶段只读本机源码、脚本、已有小型清单及官方接口资料，记录开发规划。没有为 ppocr 改 Rust／参考工程源码、下载或运行模型、加载 DLL、编译或发布；没有操作产品进程或占用前台。长期计划中其它功能的后台构建与文件交付另行记录，不计作 ppocr 验收。此结论是设计可行性，不是准确率、性能、内存或实际 DLL 加载验收。

## 实际现状与复用边界

本机 OCR 参考工程为 `<ocr-reference>`，研究时读取的 HEAD 为 `f25c88a`；未跟踪的 `benchmark/tmp` 不作为研究输入。研究时 RustShot 仍用 [Windows WinRT OCR](../../src/recognition.rs)，在 Rust 进程内运行，尚未接入 参考工程 Agent 或其模型；这段是实施前历史，不代表当前默认提供者。

参考工程 Core（`<ocr-reference>/src/*.Core/*.Core.csproj:4`） 为 `net10.0`，依赖 RapidOcrNet 3.0.0、SkiaSharp 3.119.1 和 ONNX Runtime。现有原生包包含 ORT／DirectML／Skia DLL，没有 Core 自身可直接调用的 OCR、版面或表格 C ABI。加载 `onnxruntime.dll` 只获得推理接口，不能直接得到完整识别结果。[官方 C API](https://onnxruntime.ai/docs/get-started/with-c.html) 支持同进程创建 session、输入内存 tensor 并运行模型。

| 功能 | 可直接复用 | Rust 需要实现 |
| --- | --- | --- |
| 普通 OCR | PP-OCRv6 det／rec、匹配字典、可选 cls 模型 | 缩放／padding／归一化、DB 检测后处理、透视裁行、方向判定、CTC 解码与逆坐标映射 |
| 文字区域检测 | 同一 det 模型 | 检测框后处理和原图映射；可以省掉 cls／rec，供智能效果独立使用 |
| 原文／自动排版 | 现有规则及 fixtures 作参考 | 聚行、段落识别、阅读序、原换行与段内重排两种文本 |
| 表格 | SLANet_plus ONNX | 结构 token 解码、网格修正、字符归属、合并单元格、候选复识别及 HTML／TSV |
| 通用抠图／人像抠图 | U2NetP／MODNet ONNX | 各自不同的预处理、mask 后处理、原尺寸 alpha 合成 |
| 下载和切换模型 | 已固定的模型来源、版本、字节数、SHA、profile | 宿主无关的依赖清单、下载校验、安装与 session 切换 |

截图 UIA、自动复制、贴图选字、菜单和任务取消属于宿主逻辑，继续由 RustShot 实现。公式／翻译等外部服务不能因接入 ppocr 模型就标记完成。

## OCR 移植时必须保留的行为

OnnxMobileOcrEngine（`<ocr-reference>/src/*.Core/Pipeline/OnnxMobileOcrEngine.cs:272`） 除 det→cls→rec，还包含小图单行门控、长图／小图自适应、空结果与细长稀疏重试；后续墨迹紧框、去重和相邻框分割也在 C#。RapidOcrNet 负责检测轮廓／unclip、裁行和 CTC 等基础算法，不能只移植 Core 外层调用。其[固定版本识别器源码](https://github.com/BobLd/RapidOcrNet/blob/590d44b2efc7ecbef446c36bb6359ebbfcffeb50/RapidOcrNet/TextRecognizer.cs)及[词框投影](https://github.com/BobLd/RapidOcrNet/blob/590d44b2efc7ecbef446c36bb6359ebbfcffeb50/RapidOcrNet/CalRecBoxes.cs)可作为算法参考；词／字位置由解码时间步估计，不能当作逐字轮廓真值。

内存图像接入还需保留透明输入语义：普通OCR在 CompositeTransparencyOnWhite（`<ocr-reference>/src/*.Core/Pipeline/OnnxMobileOcrEngine.cs:685`） 合成白底，检测和表格则通过 OcrImageLoader.LoadFlattened（`<ocr-reference>/src/*.Core/Pipeline/OcrImageLoader.cs:10`） 实现同类处理。Rust 的内存入口应明确 BGRA、stride、straight／premultiplied alpha，识别用白底副本，不改变贴图、底图或导出像素；抠图仍保留原alpha再乘mask，不能共用已经白底化的识别输入。此项为源码语义核对，尚未做采样等价验证。

当前 OcrLayoutAnalyzer（`<ocr-reference>/src/*.Core/Pipeline/OcrLayoutAnalyzer.cs:8`） 使用同行左→右、逐行上→下，`ColumnCount=1`，不重建报纸双栏。`FullText` 等于保留视觉换行的 `FullTextOriginal`；`FullTextAuto` 是另一种段内重排结果。旧参考实现文档或 DTO 的多栏注释不能代替当前实现。

宿主设置与Core canonical结果必须分开：参考实现结果窗（`<screenshot-pro>/ImageDrawing/Ocr/OcrResultViewModel.cs:40`）默认Auto，显示取`FullTextAuto`、复制当前显示文本；直接复制（`<screenshot-pro>/ImageDrawing/Windows/FullScreenImageWindow.xaml.cs:5181`）也优先Auto。Rust若对齐此行为，应独立保留视觉行／词框与展示／复制文本，不能用Auto全文替换用于选字与UIA合并的原图视觉行合同。研究阶段不改变当前默认设置。

普通 OcrTextBlock（`<ocr-reference>/src/*.Core/Contracts/OcrTextBlock.cs:6`） 只有一条检测文本、原图四边形及行／段落索引；`UseCharBoxes` 只把内部词框取联合用于改善框，普通结果未输出词列表。表格路径才输出更细的字符／词片段。

RustShot 的 [RecognizedText](../../src/recognition.rs:9) 已有词矩形、行矩形、全文和文字区域；[UIA 合并](../../src/hybrid_text.rs:29) 校验视觉行全文及词框。贴图 [from_recognized](../../src/pin_text_selection.rs:86) 将词映射到原文字节范围，词不完整时在完整行框上回退；现有字符停点仍是按 grapheme 等分框的估计。直接把 参考工程普通 Blocks 当 words 会丢失词粒度。

建议先建立提供者无关的结果：视觉行、词／字符片段、四边形、置信度、段落及 original／auto 文本；保存几何来源和估计性质。适配现有消费者时规范 CRLF→LF，处理段间空白行的索引，保留 ROI 局部物理坐标。重排文本不能驱动原图选字；旋转四边形转轴对齐矩形时必须明确精度损失。文字区域检测可独立接入，当前智能效果与全文识别共用的路径不必一直执行 rec。

参考工程现有`DetectRegions`省略cls／rec推理，但构造器（`<ocr-reference>/src/*.Core/Pipeline/OnnxMobileOcrEngine.cs:45`）仍初始化整组模型。Rust应独立按需创建det session，才能同时节省检测任务的模型加载与内存；不能把“未执行rec”写成“未加载rec”。

检测与全文识别的后处理参数也不同：DetectRegionsBitmap（`<ocr-reference>/src/*.Core/Pipeline/OnnxMobileOcrEngine.cs:202`）使用`DetectUnClipRatio`默认1.0，返回接近原始DB轮廓，不执行墨迹紧框／字符框后处理；普通OCR配置（`<ocr-reference>/src/*.Core/Pipeline/OnnxMobileOcrEngine.cs:59`）的`UnClipRatio`默认1.6。即使复用同一det session，仍需分别保存任务的后处理profile，不能只停掉rec而沿用全文框参数。

独立检测还需要单独的结果合同：现 det-only 输出（`<ocr-reference>/src/*.Core/Pipeline/OnnxMobileOcrEngine.cs:225`）的 `Text` 为空，但四边形和检测置信度有效；Pro 智能马赛克映射器（`<screenshot-pro>/ImageDrawing/Ocr/SmartMosaicRegionMapper.cs:34`）过滤空文字。Rust 后续接入检测时使用独立 `DetectionResult`／任务类型，按合法几何与检测阈值接收候选，不能照搬普通 OCR 的非空文字过滤，也不能伪造文字、词框或视觉行。此为提供者适配风险，不表示当前检测模型故障或必须使用 IPC。

## 表格与抠图单独接入

表格引擎（`<ocr-reference>/src/*.Core/Pipeline/OnnxTableStructureEngine.cs:151`） 将图像等比缩放至488，并使用 ONNX metadata 中的字符表解码结构。随后 TableStructureComposer（`<ocr-reference>/src/*.Core/Pipeline/TableStructureComposer.cs:297`） 做网格修正、文字归属及合并格输出，最多对 `min(32, ceil(cells*20%))` 个候选单元格复识别。初版直接套 SLANet 输出不等于现有完整表格能力。

模型输入通道顺序须逐个保留：SLANet CreateInput（`<ocr-reference>/src/*.Core/Pipeline/OnnxTableStructureEngine.cs:191`）明确按BGR填tensor；U2NetP和MODNet为RGB，且各自尺寸／归一化不同。Rust可共用原图读取与内存格式合同，但不能用同一个RGB tensor工厂直接套全部模型。此项仅为源码核对，采样／推理等价仍待实施验证。

U2NetP 使用固定320输入和 mask 全图 min/max 归一化；MODNet 使用保持比例／32对齐输入和 matte clamp，两者不能共用同一套预后处理。CutoutImageComposer（`<ocr-reference>/src/*.Core/Pipeline/CutoutImageComposer.cs:35`） 将 mask 上采样至原图，保留 RGB、原 alpha 乘 mask。Rust 可直接返回原尺寸直 alpha BGRA，不必经过 PNG／Base64 再解码。

参考工程抠图的 DML session 创建／运行失败会重建 CPU 并重跑；普通 OCR／表格目前仅有添加 provider 时的回退，不能统一宣传已有完整运行期 fallback。首版 OCR 使用 CPU，DirectML 后续再独立接入。[官方 DML 限制](https://onnxruntime.ai/docs/execution-providers/DirectML-ExecutionProvider.html)要求关闭 memory pattern、顺序执行，且同一 session 不并发 Run。

## 进程、缓存与响应

现 Agent 协议（`<ocr-reference>/src/*.Core/Agent/OcrAgentProtocol.cs:14`） 使用 Named Pipe v4；图像通过 FilePath 或 Base64 传入。其价值是跨截图保留模型和隔离故障，IPC 不是 ONNX 的要求。Rust 使用已有内存图像可减少编码、解码、序列化及传输，但本次没有测量这些环节占比。

独立源码复核还确认了可移除的额外路径：Agent检测请求（`<ocr-reference>/src/*.Agent/Program.cs:343`） 收到图像字节后先写临时PNG，再调用文件检测，最后删除。Rust直接使用已有内存图像可以同时省去这次临时文件和编码／解码路径；本次没有测量这些成本，不能据此宣称具体延迟收益。普通OCR／检测／表格已有 CLI直接构造Core引擎（`<ocr-reference>/src/*.Cli/Program.cs:361`） 路线。CLI 的 --cutout 入口（`<ocr-reference>/src/*.Cli/Program.cs:39`） 当前仍调用 Agent，但 Core 的 U2NetP（`<ocr-reference>/src/*.Core/Pipeline/U2NetpCutoutEngine.cs:21`） 和 MODNet（`<ocr-reference>/src/*.Core/Pipeline/ModNetCutoutEngine.cs:28`） 可直接创建推理会话并处理图像；这支持 Rust 移植绕过 Agent，不能写成现 CLI 抠图路径已经免 IPC。没有发现算法上必须使用Named Pipe的功能。

推荐首版按需创建活动截图／贴图进程的 OCR 后台引擎，同一会话复用 session；退出时释放。托盘继续只管理入口，待命截图进程不预载 OCR，首次可框选的约200ms链路不等待模型加载。已建立的应用 GPU 会话不带回待命进程。

**截图进程退出后缓存也会消失。** 去掉 IPC 不会消除下次模型初始化；首版必须分别记录模型冷加载、同会话重复识别、推理、结果展示和提交内存。以后若确需跨截图保留模型，先评估待命进程纯 CPU 预热的新增内存及并发影响，或有明确收益时采用独立 Rust OCR 服务；两者都是后续架构选择，不能借此常驻保留已初始化的截图 GPU 进程。跨进程共享模型／驱动故障隔离才可能构成保留通信的实际理由。

长生命周期贴图只缓存当前所需模型，切换、空闲和关闭时有释放策略。不要照搬 AgentEngineCache（`<ocr-reference>/src/*.Agent/AgentEngineCache.cs:14`） 长期缓存多套 OCR／表格引擎的方式。后台线程和有界任务队列避免阻塞 GPUI；取消、重试、来源变化继续用会话身份拒绝迟到结果。

失败初始化也须纳入所有权：现 OCR warmup（`<ocr-reference>/src/*.Agent/AgentEngineCache.cs:102`）及表格 warmup（`<ocr-reference>/src/*.Agent/AgentEngineCache.cs:147`）在成功入缓存前没有显式的失败 Dispose 保护，抠图（`<ocr-reference>/src/*.Agent/AgentEngineCache.cs:294`）则已有保护；此为源码缺少释放保证，尚未实测泄漏。Rust 用 RAII 持有候选 session，初始化全部成功后才发布；失败、取消或来源失效时释放未发布对象。实际 Run 未结束时，由推理任务继续持有 session／tensor，界面撤销结果许可不能同时销毁仍在计算的资源。

补充取消边界：参考工程普通OCR（`<ocr-reference>/src/*.Core/Pipeline/OnnxMobileOcrEngine.cs:248`）仅入口检查 `CancellationToken`，随后同步识别；表格 Predict（`<ocr-reference>/src/*.Core/Pipeline/OnnxTableStructureEngine.cs:124`）及抠图 session.Run（`<ocr-reference>/src/*.Core/Pipeline/DirectMlFallbackSession.cs:146`）未传入每请求 `RunOptions`。抠图虽在推理前后检查取消，也不能据此声称原生 Run 可立即停止。Rust 实施需分别处理排队取消、原生计算终止和迟到结果拒绝；丢弃 GPUI 后台任务不等于同步推理已结束。[ORT 的 RunOptionsSetTerminate](https://onnxruntime.ai/docs/api/c/struct_ort_api.html)提供从另一线程请求终止正在运行推理的接口，但具体绑定、模型／provider 的响应和资源释放仍待验证，不能承诺即时中断；未来 DML 回退须区分主动取消和设备故障，取消不应触发 CPU 重跑。

补充线程／内存预算：OCR、表格与抠图选项均默认 `NumThreads=4`，本机 Core 源码未显式配置共享线程池或 arena。OCR 的 GetSmallImageRecognizer（`<ocr-reference>/src/*.Core/Pipeline/OnnxMobileOcrEngine.cs:751`）还会另建 rec 引擎，默认 warmup 条件会调用它；不能仅按 det／cls／rec 三个 session 估算占用。Rust 应先评估复用 rec，并限制整个工作进程的任务并发与线程预算。[ORT C 官方说明](https://onnxruntime.ai/docs/get-started/with-c.html)指出默认每 session 有线程池／arena，支持共享配置，arena 高水位也可能在会话内保留；共享、裁剪和模型释放均需按实际采用配置测量，当前没有 Rust 内存或性能数据。本次仅补中文研究及开发记录，没有修改源码／Cargo、加载 DLL、运行模型或重新编译。

保留 Agent 也不会自动获得推理取消：客户端 token（`<ocr-reference>/src/*.Core/Agent/OcrAgentClient.cs:159`）用于连接、写入和等待读回，服务端普通识别调用（`<ocr-reference>/src/*.Agent/Program.cs:355`）没有接收该 token，现协议没有逐请求取消命令。因此取消能力应在 Rust 任务与原生 Run 两层实现，不能把更换通信架构当作取消已经解决。

## 原生库兼容与分发

现有 native-1.0.0 清单（`<ocr-reference>/dist/*-native-1.0.0/payload/manifest.json:13`） 是 win-x64、ORT 1.24.4、DirectML 1.15.4；Core 当前 Managed 包为1.27.0，不能据此认定原生 DLL 是1.27。现有 managed／native 组合本次未运行，不推断成功或失败。

补充实际 PE 文件核对：四个DLL均AMD64／PE32+，大小及SHA全部与清单相符；payload只有清单、说明和这四件DLL。`onnxruntime.dll`静态导入 `MSVCP140.dll`、`MSVCP140_1.dll`、`VCRUNTIME140.dll`、`VCRUNTIME140_1.dll`，四件均未随包包含，目标机是否已有兼容版本尚未核实。[ORT Windows安装要求](https://onnxruntime.ai/docs/install/)明确依赖VC运行库，后续便携依赖清单需覆盖其版本／架构和部署来源，按[微软分发规则](https://learn.microsoft.com/en-us/cpp/windows/determining-which-dlls-to-redistribute?view=msvc-170)准备分发材料；系统API sets不当作可随意复制的用户DLL。

ORT中的 `DirectML.dll`／`d3d12.dll`／`dxgi.dll`位于延迟导入表；`onnxruntime_providers_shared.dll`未出现在其静态或延迟表，[固定版本源码](https://github.com/microsoft/onnxruntime/blob/v1.24.4/onnxruntime/core/session/provider_bridge_ort.cc#L1637)在相关provider注册路径动态加载shared。CPU首版可研究裁掉Skia、DirectML和shared，只保留ORT、兼容x64 VC运行库及系统依赖；当前DML包裁剪后的加载／CPU执行未验，不能认定最小组合已经成立。此核对仅解析文件，没有加载DLL、创建session或运行模型；未递归解析VC依赖或穷举动态加载路径。

复用该原生包时固定 API24 和明确 DLL 路径；[ORT 1.24.4 header](https://github.com/microsoft/onnxruntime/blob/v1.24.4/include/onnxruntime/core/session/onnxruntime_c_api.h) 定义 API24。[GetApi](https://onnxruntime.ai/docs/api/c/struct_ort_api_base.html) 在请求不支持版本时返回空。可研究的 Rust 绑定候选如下，**尚未修改 Cargo 或编译验证**：

```toml
ort = { version = "=2.0.0-rc.13", default-features = false, features = ["std", "load-dynamic", "api-24"] }
```

[rc.13 发布源码](https://github.com/pykeio/ort/blob/v2.0.0-rc.13/Cargo.toml) 要求 Rust1.88，当前 RustShot 为1.90；它默认开启 API27及自动下载，应关闭默认 features，不使用未固定的 latest。`ndarray` 按接口需要增加；CPU 初版无需启用 `directml`。依赖的真实 MSRV、API协商及运行仍待实施验证。

本次复核补齐原生库生命周期：参考工程的 NativeRuntimeLibraries.Use（`<ocr-reference>/src/*.Core/Native/NativeRuntimeLibraries.cs:88`） 在进程绑定后拒绝另一目录；候选 Rust [ort rc.13 的实现](https://github.com/pykeio/ort/blob/v2.0.0-rc.13/src/lib.rs#L110)也以进程级 `OnceLock` 保存 DLL 和 API。释放模型 session、切换 Tiny／Small，不代表可在同一个贴图进程热切 ORT DLL。实施时一个活动进程固定一个原生版本；依赖按版本目录保存，新下载的原生版本供后续进程使用，不能覆盖正在使用的 DLL。模型切换仍需另验输入、词表、任务和缓存身份，不能与原生库更新混为一项。

加载失败或不支持的 API 应作为可显示的识别错误处理，先显式校验并加载固定绝对路径，再使用会触发全局初始化的 ORT 接口；候选绑定的部分隐式初始化路径会 panic，RustShot Release 的 `panic = "abort"` 不允许靠捕获 panic 恢复。后台线程能避免阻塞 UI，但不提供原生崩溃或卡死隔离。确有“不兼容原生版本必须同时运行”或“原生任务卡死仍须保住当前编辑会话”的实测需求时，再评估独立 Rust 推理进程；当前没有此类实测证据，默认方案仍为进程内 CPU。以上是静态设计约束，没有加载 DLL、运行模型或改变运行架构。

继续按运行库、OCR模型、表格模型、抠图模型拆分可下载依赖，复用固定来源／SHA／字典配对，不让目标机器依赖 Quicker、.NET、MCP 或某个 Quicker 版本。Tiny／Small 不能混用字典；参考工程 CLI 默认 Tiny、参考实现宿主默认 Small，是不同产品设置。可先用 Tiny 完成管线，再验证同管线 Small；RustShot 最终默认档位按实际准确率／内存确定，不在本次研究中切换。

档位选择也影响语言覆盖：[固定版本PaddleOCR官方说明](https://github.com/PaddlePaddle/PaddleOCR/blob/b03f46425e8ff4442b268ce449e3eef758146cd4/docs/version3.x/algorithm/PP-OCRv6/PP-OCRv6.md)列Tiny为49种、不含日文，Small／Medium为50种、含日文；这不是Rust移植后的准确率验收。实施时应在profile中明确模型、词表及语言支持，不能只按速度／内存选档。算法上的“可选cls”也不等于现有包缺件可接受：pack-models.ps1（`<ocr-reference>/scripts/pack-models.ps1:44`）仍要求det／cls／rec／dict四件，省略分类应是明确的Rust profile，而非显式路径不存在后静默跳过。

ORT 为 MIT、Rust `ort` 为 MIT／Apache-2.0；RapidOcrNet [3.0.0 包](https://www.nuget.org/packages/RapidOcrNet/3.0.0) 标 Apache-2.0，移植时保留来源和相关通知。DirectML 有 [Microsoft 专用许可](https://www.nuget.org/packages/Microsoft.AI.DirectML/1.15.4/License)，不能统称 MIT；模型按各自已有 provenance／许可配对。Rust 图像处理可使用现有库，不必绑定 SkiaSharp；采样差异另验。资产清单和通知是实际分发材料的一部分，不能仅复制四个 DLL 就声称包完整。

## 后续实施顺序与未验范围

1. 统一内存图像、视觉行／词框、original／auto展示／复制及任务生命周期合同，按需加载固定 ORT CPU；检测任务单独det session，模型／词表／cls策略由明确profile校验。
2. 移植 Tiny 的 det→crop→rec→CTC、词／字符片段和逆坐标映射，接现有截图／贴图识别结果界面。
3. 补方向、自适应／重试、紧框／去重及当前阅读序，再验证 Small；检测区域供智能效果复用。
4. U2NetP、MODNet 按各自 pipeline 接抠图；SLANet 与 Composer 单独接表格，提供 HTML／TSV。

本次未验：DLL加载／ABI协商、模型运行、取消中断、算法与采样等价、中文字及各模型支持语言的准确率、词框选字精度、冷启动／推理／内存、DML设备丢失和回退、异机首次下载。按变化验证相关自有 fixtures，真实交互不占用户前台，也不作为后台实现前置条件。研究阶段没有接入ppocr或因研究运行Cargo；同日随后按长期计划独立完成普通OCR重新识别，仍使用WinRT，其会话守卫可供后续提供者接线，不能据此标记ppocr已实现。

本日独立复核已补充宿主默认Auto、各档语言覆盖、cls包完整性和det加载边界；主方案不变，现有研究足够进入后续实施规划，不再扩大为新的研究任务。当前用户要求仍是“先研究”，没有新增ppocr依赖、模型或引擎实现。
