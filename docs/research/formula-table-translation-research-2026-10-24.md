# 公式、表格识别与翻译方案调研

调研日期：2026-10-24。本记录保留 RaiShot 选型时查阅的公开产品与第三方方案；研究结论随源码公开，当前实现与后续验证以项目专题和状态页为准。

后续本机A/B已完成，最终推荐与真实数据以[公式与离线翻译实测结果](model-ab-results-2026-10-24.md)为准。本文件保留前期候选与资料边界。

用户随后将本地包保留为候选，优先考虑API；最新接入建议见[DeepSeek API实测](deepseek-api-results-2026-10-24.md)。

本次核实官方源码、文档、论文入口与模型文件元数据，并对照当前工程；没有下载模型、运行识别、调用付费 API 或测量 Windows 性能。下文“推荐”是入围顺序，不能替代真实截图 A/B。MB/GB 使用十进制；明确区分模型文件、压缩下载、附加运行时和运行内存。未测内存不按文件大小推算。

## 1. 先给选择建议

**推荐以“轻量本地表格 + 按需本地公式 + 在线/离线可选翻译”为主线。普通截图不加载这些模型，困难样本再由用户选择增强识别。**

| 能力 | 推荐入围 | 选择理由 | 尚需证明 |
| --- | --- | --- | --- |
| 日常表格 | 保留现有 SLANet_plus + 本机 OCR + 像素线/坐标规则 | 结构模型仅约 7.76 MB，已完成 Rust/ORT 集成，最小增量成本 | 真实模型端到端、无线/合并格、Excel/WPS 粘贴 |
| 复杂表格 | 先评测仅 UNet 有线增强；无线难题再比较 LORE、SLANeXt | UNet 仅约 8.34 MB；完整分类+UNet+LORE 约 118.69 MB，公开比较并不支持整体替换 plus | Rust 后处理移植、现有失败样本上的实际收益 |
| 公式，均衡档 | PP-FormulaNet_plus-S 与 TexTeller3 INT8 两者 A/B | 前者约 260 MB 原生包且有真实服务端工程；后者约 319 MB、ONNX 路径明确 | 前者 Windows ORT 一致性；后者量化精度、复杂公式和解码延迟 |
| 公式，轻量档 | RapidLaTeXOCR，约 179 MB 模型文件 | ONNX 路线清楚，适合裁剪后的打印公式 | 多行、矩阵、中文混排和手写边界 |
| 普通文字翻译 | 保留 Index 在线入口，补服务鉴权与逐段校验 | 用户免配置，客户端不携带翻译模型 | 公网稳定性、限流及服务使用边界 |
| 离线翻译，均衡档 | Hy-MT2-1.8B Q4 与 Index-Translate-2B Q4 两者 A/B | 官方量化文件约 1.13 / 1.31 GB；适合术语、上下文与结构化翻译候选 | Windows CPU 首字/整段延迟、质量、内存及空闲释放 |
| 离线翻译，极小档 | OPUS-MT/Marian INT8，按语言方向下载 | 可望比小型 LLM 更省空间与内存 | 量化成品实际大小、技术文本质量；百 MB/方向只是预算 |
| 困难样本增强 | Mathpix 公式、腾讯/百度表格、用户自备视觉模型 | 不增加本地模型，适合低频高难度内容 | 网络、费用、上传范围、结果可校正性 |

目前最有把握的决定是保留轻量表格基础。公式与离线翻译仍各保留两个主要候选，**不宜仅凭官方分数直接定“最好”**。准确率高一点但要频繁等待、修正、下载或重新配置，最终体验可能更差。

## 2. Snow Shot 实际采用什么

### 2.1 普通 OCR、专用识别、视觉模型是三条路径

普通文字 OCR 在本机。当前公开主分支用 simd-paddle-ocr-rs，默认 PP-OCRv6 Small，CPU worker 与模型按需获取；这不是表格和公式的全部推理环境。[官方 OCR 资源说明](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/packaging/ocr-models.md)

专用表格/公式识别上传处理后的 WebP，客户端调用 /api/v1/table/extract 与 /api/v1/latex/extract，分别读取 HTML、LaTeX。图片最长边 2880、WebP quality 75；客户端的 35/65 秒是请求期限，不能当识别耗时。另有视觉模型识别两类内容及转 Markdown/HTML 的入口。[客户端 API 实现](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/src/network/snowshotapiclient.cpp)

官方隐私政策也明确：表格、公式使用识别服务器；AI 转换和翻译可以转发给配置的 AI 服务商。不能将“小安装包、免安装模型”解释成这些能力都在本机运行。[隐私政策](https://snowshot.top/zh/privacy.html)

### 2.2 公式后端：PP-FormulaNet_plus-S

作者的 [SnowShotApi](https://github.com/mg-chao/SnowShotApi) 公开代码有独立公式 worker：Python 3.12、PaddleX 3.7.2、PaddlePaddle 3.3.1，Linux/WSL CPU 的 Paddle 静态推理；面向“裁剪后的单个打印公式”。模型 tar 固定 259,604,480 bytes，约 259.6 MB，另有 Python/Paddle 环境。[worker 说明](https://github.com/mg-chao/SnowShotApi/blob/main/services/FormulaRecognition/README.md)、[模型清单](https://github.com/mg-chao/SnowShotApi/blob/main/services/FormulaRecognition/model-manifest.json)

可借鉴的体验细节：深色底先归一化；未生成 EOS 的截断结果算失败；不靠规则擅自“修正”数学内容。该 worker 自己的挑战样本仍有括号、重复符号、微分/分数错误，因此“接口成功”也不能代表公式正确。文档明确说本地替换未改变线上服务器，故这里只确认公开代码方案，未确认其线上已全部切换。[验证与已知限制](https://github.com/mg-chao/SnowShotApi/blob/main/services/FormulaRecognition/README.md#verification-and-accuracy)

### 2.3 表格后端：分类 + OCR + 有线/无线专用模型

当前公开 worker 先判断表格类型，OCR 一次，再选择有线或无线结构引擎，输出 HTML；支持 Windows DirectML 与 Ubuntu CPU。它不是直接调用 SLANet_plus。结构链路为 YOLO 分类 → 有线 UNet，或无线 LORE detect/process；文字使用 PP-OCRv6 Small，并请求词级框。[worker 说明](https://github.com/mg-chao/SnowShotApi/blob/main/services/TableStructureRec/README.md)、[引擎实现](https://github.com/mg-chao/SnowShotApi/blob/main/services/TableStructureRec/table_rec_service/engine.py)、[固定模型清单](https://github.com/mg-chao/SnowShotApi/blob/main/services/TableStructureRec/model-manifest.json)

固定模型文件元数据与清单对应：分类器 21.78 MB，UNet 8.34 MB，LORE detect/process 为 44.33/44.24 MB；四者合计 118,690,520 bytes，约 118.69 MB。加其 OCR 三模型后为 150.44 MB，仍不含 Python、ORT 和图像/几何处理依赖。若复用本工程 OCR，可研究仅增加结构部分；若用户明确选有线表，也不一定需要下载分类器和无线模型。[上游模型文件库](https://www.modelscope.cn/models/RapidAI/RapidTable/files)

### 2.4 比模型更值得借鉴的结果处理

表格有独立文档模型，保留行列跨度、单元格编辑、合并/拆分、区域复制与 HTML/纯文本输出。公式识别与预览渲染分开，客户端存在本地 MicroTeX 渲染路径；“识别模型输出 LaTeX”和“用户能看懂、修正、复制公式”是两件事。[表格文档接口](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/include/snow_shot/presentation/screenshottabledocument.h)、[公式预览实现](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/src/presentation/ocr/screenshotlatexrenderer.cpp)

文字翻译配置可选 DeepL、DeepLx、百度、有道，另有 AI/自定义模型路径。DeepLx 是兼容服务选项，不能据此视为 DeepL 官方、稳定免费的生产 API。[配置约束](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/include/snow_shot/texttranslationconfiguration.h)、[协议实现](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/src/network/texttranslationprotocol.h)

官方后端公开默认配置走阿里云 DashScope：纯文本为 qwen3.7-flash，图像为 qwen3.8-flash；专用翻译默认也选前者、关闭 thinking。qwen-mt-flash 等公共别名会经路由解析，不能单凭别名断言实际使用专用 Qwen-MT。部署可以覆盖默认值，这些信息不是此刻线上模型探测结果。[默认配置](https://github.com/mg-chao/SnowShotApi/blob/main/src/SnowShotApi/appsettings.json)、[模型路由](https://github.com/mg-chao/SnowShotApi/blob/main/src/SnowShot.Infrastructure/Production/Configuration/ProviderModelCatalog.cs)

翻译客户端校验索引 JSON，并要求保留格式、占位符、代码和 URL。官方限流记录指出逐文本框请求与共享公网 IP 会影响长页面体验，后续采用共享任务槽、起发节流和明确 429 冷却。这说明请求颗粒度、缓存与重试策略值得和模型质量一起优化；不必照搬其服务限额。[上游翻译客户端](https://github.com/mg-chao/SnowShotApi/blob/main/src/SnowShot.Infrastructure/Production/Providers/OpenAiTranslationClient.cs)、[限流设计](https://github.com/mg-chao/SnowShotApi/blob/main/docs/translation-admission-20261022.md)

## 3. 当前工程已有基础

以下来自当前源码和本机记录，不是本次重新运行的验证结果。

- 表格：已有 SLANet_plus 固定 ONNX，7,758,305 bytes（7.76 MB / 7.40 MiB），共享 ORT；Rust 完成预处理、结构解码、跨度、OCR 填字、规则择优和失败降级。已有 TSV、CF_HTML、Markdown 及单元格覆盖显示。入口见 [模型清单](../../dependencies/ppocr-table-cpu.json)、[后台任务](../../src/ppocr/table.rs)、[已有专题](table-recognition-2026-10-12.md)。该专题仍把首次真实模型/真实粘贴列为未验，不能以规则夹具通过替代模型效果证明。
- 翻译：默认 Index-Translate 公网兼容接口，支持流式；设置目前主要为 endpoint/model/target，纯文本请求没有 API key/Bearer，所以“可改端点”不等于已经完整支持一般 BYOK 云服务。[当前实现](../../src/translate.rs)
- 图片翻译：已有本机 OCR→分块→翻译→可编辑文字标注，也有用户要求的百度 V2 整图接口。前者只需发送文字，后者发送选区图并接收译图位图，数据发送和可编辑语义不同。[图片翻译专题](../references/image-translate-2026-10-12.md)、[百度接口代码](../../src/baidu_image_translate.rs)
- 公式：开发计划仍为待规划；识别后的本地预览、Word 可编辑公式、MathML 不能算现成能力。[当前计划](../roadmap.md)

## 4. 公式识别候选

除 Mathpix 外，下列主要目标是图片转 LaTeX；不默认具备 Word 公式、MathML 和完整文档排版还原。

| 候选 | 模型/下载规模；附加依赖另计 | 效果定位与主要限制 | 集成和取舍 |
| --- | --- | --- | --- |
| RapidLaTeXOCR | 约 179 MB：encoder 89 MB、decoder 51 MB、resizer 39 MB、tokenizer；官方 ZIP 约 164 MB | 裁剪打印公式基线；默认最大 672×192、512 tokens，复杂多行/中文需验 | 原作者有 ONNX，Rust 可共享 ORT，最适合小体积基线。[项目](https://github.com/RapidAI/RapidLaTeXOCR)、[发行文件](https://github.com/RapidAI/RapidLaTeXOCR/releases/tag/v0.0.0)、[配置](https://github.com/RapidAI/RapidLaTeXOCR/blob/main/rapid_latex_ocr/config.yaml) |
| PP-FormulaNet_plus-S | 官方表标称 248 MB；Snow 固定 tar 约 259.6 MB；HF 原生目录约 264 MB，三者口径不同 | 英文公式为主；中文混排弱于 M/L；有真实 worker 工程可参考 | 优先 A/B，但 Paddle 原生包不能直接当 ORT 插件。[官方文档](https://github.com/PaddlePaddle/PaddleOCR/blob/main/docs/version3.x/module_usage/formula_recognition.en.md)、[原生文件](https://huggingface.co/PaddlePaddle/PP-FormulaNet_plus-S/tree/main) |
| TexTeller3 INT8 动态 ONNX | encoder 88,254,758 + merged decoder 229,322,140 + tokenizer 1,370,259 bytes，合计约 319 MB | 原模型覆盖手写、扫描、中英混排与多行；量化版是否保住精度待测 | 第三方导出，支持 KV cache；有 Rust 实现可参照，不依赖用户装 Python。[量化作者模型卡](https://huggingface.co/Ji-Ha/TexTeller3-ONNX-dynamic)、[Rust 实现](https://docs.rs/bobine/latest/bobine/tex_teller/struct.TexTeller.html) |
| PP-FormulaNet_plus-M / L | 官方标称 592 / 698 MB；M 当前 HF 原生目录约 624 MB | M 增强中文、复杂公式，最长 2560 tokens；L 更重，收益须测 | 只作高精度可选包，不必默认安装。[M 模型卡](https://huggingface.co/PaddlePaddle/PP-FormulaNet_plus-M)、[文件](https://huggingface.co/PaddlePaddle/PP-FormulaNet_plus-M/tree/main) |
| TexTeller 原始 ONNX | 最小 encoder + merged decoder 约 1.25 GB；整个仓库 5.38 GB 含重复格式 | 可作 INT8 精度对照；段落识别不等于还原全文结构 | 已有 ONNX，但体积明显更大；INT4 约 225 MB 仅作后续量化研究。[原作者](https://github.com/OleehyO/TexTeller)、[文件列表](https://huggingface.co/OleehyO/TexTeller/tree/main) |
| pix2tex / LaTeX-OCR | 主权重约 102 MB，不含 resizer、Python/Torch 环境 | 成熟打印公式基线，低清输入仍需核对 | 若集成桌面，优先考察 ONNX 衍生路线，不把 102 MB 当总包大小。[原作者](https://github.com/lukas-blecher/LaTeX-OCR)、[权重](https://huggingface.co/spaces/lukbl/LaTeX-OCR/blob/main/checkpoints/weights.pth) |
| UniMERNet tiny / small / base | 官方标称约 441 / 773 MB / 1.3 GB；tiny 当前 HF 约 432 MB 的 .pth | 覆盖复杂打印、截图、手写；适合质量参照 | 官方 tiny 文件不是即插即用 ONNX，还要评估运行时与移植。[项目](https://github.com/opendatalab/UniMERNet)、[tiny 文件](https://huggingface.co/wanderkid/unimernet_tiny/tree/main) |
| Mathpix API | 本地无模型；当前公开价 $0.002/图，首 API key 一次性 $19.99，超过 12 行可能按 $0.005/页 | 打印、手写、文本、表格，含 LaTeX/MathML 等输出 | 适合困难样本对照；国内网络、费用、隐私设置需验。价格仅为本次页面信息。[接口](https://website.mathpix.com/docs/convert/endpoints)、[价格](https://website.mathpix.com/pricing/api) |

**Windows ORT 是 PP-FormulaNet 的首要闸门。** 官方 Paddle2ONNX issue 仍记录 plus-S 导出 InvalidGraph，以及 GPU 重复推理异常/CPU 可用的个案；这不能证明所有导出都不可用，但不能跳过导出与原生模型一致性验证。已有 [OAR-OCR Rust 实现](https://github.com/GreatV/oar-ocr) 可参考，采用前还须核对模型、预处理、tokenizer、生成停止条件及 ORT 版本。[导出问题](https://github.com/PaddlePaddle/Paddle2ONNX/issues/1639)、[重复推理问题](https://github.com/PaddlePaddle/Paddle2ONNX/issues/1593)

CPU 数字只作量级参考：Paddle 官方 plus-S/M/L 的高性能模式模型推理约 261/1616/3126 ms，机器为 Ubuntu、Xeon Gold 6271C；不含截图、前后处理、网络、渲染，也不是普通 Windows 笔记本结果。RapidLaTeXOCR README 的约 0.479 秒与 TexTeller Rust 文档的约 10 ms/token 缺统一硬件口径，不据此排名。[Paddle 测量条件](https://github.com/PaddlePaddle/PaddleOCR/blob/main/docs/version3.x/module_usage/formula_recognition.en.md)

许可：Paddle、UniMERNet、TexTeller 及所列 TexTeller 量化模型标注 Apache-2.0；仍须随具体产物保留许可。RapidLaTeXOCR 的 LICENSE/README 为 MIT，但 setup.py 标注 Apache-2.0，存在元数据不一致，要按实际代码与上游权重来源整理。[Rapid LICENSE](https://github.com/RapidAI/RapidLaTeXOCR/blob/main/LICENSE)、[打包元数据](https://github.com/RapidAI/RapidLaTeXOCR/blob/main/setup.py)。Mathpix 可显式设置 improve_mathpix=false，官方称不保存图片和 OCR 缓存，但仍有计费元数据；以服务条款为准。[隐私选项](https://docs.mathpix.com/concepts/privacy)

预览也要计入集成成本：可研究 MIT 的 C++17 MicroTeX，通过 FFI/图形后端桥接；也可先评测 MIT 的纯 Rust RaTeX（display list、PNG/SVG）。两者都不要求 WebView，但本次没有构建或验证其 GPUI 适配、复杂宏覆盖和复制语义，不将其当作已完成的组件。[MicroTeX](https://github.com/NanoMichael/MicroTeX)、[RaTeX](https://github.com/erweixin/RaTeX)

## 5. 表格识别候选

结构模型负责“格子在哪里、如何合并”，OCR 负责“格子里是什么字”。结构分数不是整表文字正确率。表格线/坐标规则、结构模型、单元格填字与输出格式需要一起验证。

| 候选 | 规模/运行时 | 适用性与建议 |
| --- | --- | --- |
| 现有规则 + SLANet_plus | 结构增量约 7.76 MB，共享 OCR/ORT | 默认保留；对无线/复杂表不足时再增强。RapidTable README 的 6.8M 与当前固定转换件 bytes 不同，交付以实际清单为准，未核实差异原因。[RapidTable](https://github.com/RapidAI/RapidTable)、[默认模型映射](https://github.com/RapidAI/RapidTable/blob/main/rapid_table/default_models.yaml) |
| UNet 有线增强 | ONNX 约 8.34 MB，共享现有 OCR/ORT | 优先于整套替换做小规模验证；另需1024输入、横竖线分割、形态学、连通域与单元格拓扑后处理。[上游 TableStructureRec](https://github.com/RapidAI/TableStructureRec)、[UNet 实现](https://github.com/mg-chao/SnowShotApi/blob/main/services/TableStructureRec/wired_table_rec/table_structure_unet.py) |
| UNet + LORE + 分类器 | 结构/分类四 ONNX 共约 118.69 MB，完整七模型约 150.44 MB | Snow 采用的路线。需要移植分类、图像/逻辑结构后处理、OCR 对齐，不能只加载四个 session。[Snow worker](https://github.com/mg-chao/SnowShotApi/blob/main/services/TableStructureRec/table_rec_service/engine.py)、[LORE 后处理](https://github.com/mg-chao/SnowShotApi/blob/main/services/TableStructureRec/lineless_table_rec/main.py) |
| 旧 SLANet | 中/英文结构约 7.3/7.4M（上游标称） | 与 plus 同档，缺乏倒退替换的理由。[RapidTable 模型对比](https://github.com/RapidAI/RapidTable) |
| SLANeXt wired / wireless | 官方 351 MB/个；两路约 702 MB，另需 OCR | 可作困难表本地增强；官方 69.65% 为两模型联合结构测试，不是整表正确率。[结构模块文档](https://github.com/PaddlePaddle/PaddleOCR/blob/main/docs/version3.x/module_usage/table_structure_recognition.en.md)、[wired 模型卡](https://huggingface.co/PaddlePaddle/SLANeXt_wired) |
| Table Recognition v2 / PP-TableMagic | 两个 SLANeXt + 两个 RT-DETR-L 单元格模型 + 分类，官方体积相加约 956.6 MB；未含 OCR/版面/运行时 | 完整分类与结构/单元格融合路线，适合专业增强，不宜只按单个 351 MB 文件比较。[官方管线](https://www.paddleocr.ai/main/en/version3.x/pipeline_usage/table_recognition_v2.html) |
| PP-StructureV3 | 多模型，依配置变化 | 更适合整页文档解析，手工已框单表时没必要全开；当前 ORT 示例关闭公式识别，不代表全套模型均能直接复用 ORT。[官方说明](https://www.paddleocr.ai/main/en/version3.x/pipeline_usage/PP-StructureV3.html) |
| Microsoft Table Transformer | 结构权重约 110–115 MB，另需 OCR、检测/后处理 | 有结构框与公开评测，训练场景主要论文/财报；当前没有明显替换轻量方案的收益证据。[官方仓库](https://github.com/microsoft/table-transformer)、[权重文件](https://huggingface.co/microsoft/table-transformer-structure-recognition-v1.1-all/tree/main) |
| UniTable | RapidTable 标称约 500M，另需 Torch 等 | 自回归结构方案，可作质量研究，暂不优先客户端集成；RapidTable 引用的转换权重与原始 MIT 许可需分别核对。[UniTable](https://github.com/poloclub/unitable)、[相关转换实现](https://github.com/Sanster/OhMyTable) |
| PaddleOCR-VL-1.6-0.9B | 官方核心权重约 1.92 GB，另需模型服务运行时 | 可统一文字、公式、表格和混排；适合已有本地/内网模型服务的高级用户，不作小体积基础包。[模型卡](https://huggingface.co/PaddlePaddle/PaddleOCR-VL-1.6)、[文件](https://huggingface.co/PaddlePaddle/PaddleOCR-VL-1.6/tree/main) |
| 腾讯表格 V3 | 无本地模型，图片上传 | 复杂表、多表、嵌套、无线、旋转，回 Excel；UseNewModel=true 更强但稍慢，坐标行为需单独验。Base64 后 10 MB、建议长宽比<3，长截图需分块。[官方 API](https://cloud.tencent.com/document/api/866/86721) |
| 百度表格 V2 | 无本地模型，图片上传 | 有线/无线、合并单元格、多表，可返回 Excel Base64；作第二云端对照即可。[官方说明](https://ai.baidu.com/tech/ocr_others/table/)、[API](https://aca.bce.baidu.com/doc/OCR/s/Al1zvpylt) |

官方 CPU 模型推理参考：SLANet_plus 约 41.80 ms、SLANeXt 约 501.66 ms，Xeon 6271C 高性能模式；不能转写成用户点击后的等待时间。[官方表格测量条件](https://github.com/PaddlePaddle/PaddleOCR/blob/main/docs/version3.x/module_usage/table_structure_recognition.en.md)

**没有证据支持把 Snow 的整套表格路线直接判为更好。** RapidAI 官方同表对比里，分类+UNet v2+LORE 的 TEDS/结构 TEDS 为 0.77676/0.84580，SLANet_plus 为 0.84481/0.91369；官方同时建议有线场景考虑 wired_v2、文档综合场景考虑 plus。该测试也不是本工程截图分布。应先测有线增强和当前失败样本，避免为更大的模型包牺牲综合效果。[上游测评与建议](https://github.com/RapidAI/TableStructureRec/blob/main/README.md)

RapidAI 的整体 CPU 1–7秒、有线结构百毫秒/无线约500毫秒缺完整统一硬件口径。Snow 曾记录 Ryzen 5800H 上四个表格样本 HTTP 均值1.377秒、P95 1.705秒，但与公式并发、排除公网/网关、属于特定旧部署，不能用作当前 RaiShot 延迟保证。[部署测量及限制](https://github.com/mg-chao/SnowShotApi/blob/main/docs/ubuntu-cpu-recognition.md)

Paddle 的上述结构模型卡标 Apache-2.0，TATR 标 MIT；使用第三方转换权重时同时核对转换产物来源，不能仅看封装库的 LICENSE。表格输出应保留 row/column/span、空格空行、前导零、小数点、负号、数字及日期的原始文本；CF_HTML 用于合并格，TSV 用于通用粘贴，Markdown 不承担复杂跨度的保真承诺。

UNet/LORE 路线应直接查 RapidAI 上游：TableStructureRec 代码为 Apache-2.0，RapidTable 模型库同样作 Apache 声明，LORE 官方仓库根许可为 Apache-2.0；UNet 个人贡献权重与 YOLO 分类权重仍需保留来源核对。SnowShotApi 自身为 AGPL-3.0，不能把其中修改过的 worker 一律当上游 Apache 代码复制。[RapidAI 许可](https://github.com/RapidAI/TableStructureRec/blob/main/LICENSE)、[LORE 官方来源](https://github.com/AlibabaResearch/AdvancedLiterateMachinery/blob/main/DocumentUnderstanding/LORE-TSR/README.md)、[根许可](https://github.com/AlibabaResearch/AdvancedLiterateMachinery/blob/main/LICENSE)、[Snow API 许可](https://github.com/mg-chao/SnowShotApi/blob/main/LICENSE)

## 6. 翻译方案列表

### 6.1 轻量在线与自备服务

| 方案 | 体积与配置 | 体验与选择 |
| --- | --- | --- |
| Index 公网 API | 客户端无模型，当前项目默认 | 官方明确提供免费兼容 API；保留免配置入口，但未发现 SLA、商业流量与保留期限承诺，不能据权重许可保证公共服务永远免费稳定。[官方仓库](https://github.com/bilibili/Index-Translate) |
| 用户自备兼容 LLM | 本地无新增模型，需 endpoint/model/key；可连本机或局域网 | 技术文本/术语/上下文灵活；补齐鉴权、连接测试、结构校验。高级用户有价值，普通用户首次输入 key 的摩擦较大 |
| DeepL API | 无本地模型，需账号/Key | context、glossary、数组、HTML/XML 标签处理适合段落翻译。旧每月 50 万字符 API Free 已不可新购，新 Developer 为总计 100 万字符试用，不能沿用旧额度宣传。[接口](https://developers.deepl.com/api-reference/translate/request-translation)、[套餐](https://support.deepl.com/hc/en-us/articles/360021200939-DeepL-API-plans) |
| Azure Translator | 无本地模型，需账号/Key | 数组、HTML、动态词典，适合可预测的短文本批量翻译；标准请求最多 50,000 字符、1,000 元素。官方短文本延迟不是截图端到端承诺。[接口](https://learn.microsoft.com/en-us/azure/ai-services/translator/text-translation/reference/v3/translate)、[上限](https://learn.microsoft.com/en-us/azure/ai-services/Translator/service-limits) |
| Google Cloud Translation | 无本地模型，需云配置 | NMT、多语言；Advanced 有 glossary/Translation LLM；支持 HTML 标签，XML 不属同一支持范围。是否优于前两家要按实际语言方向测，不凭品牌排序。[文档](https://docs.cloud.google.com/translate/docs/basic/translating-text)、[计价](https://cloud.google.com/products/translate/pricing?hl=en) |
| 百度/有道文本翻译 | 无本地模型，账号与签名适配 | 国内服务可作区域化选项；Snow 已有对应协议，但不必首版同时接入所有厂商。具体可用地域、额度、术语能力随选购服务核实。[Snow 协议证据](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/src/network/texttranslationprotocol.h) |
| 百度图片 V2 | 已有项目接入，上传截图 | OCR、翻译、擦除/回填由服务完成；对复杂背景可能有价值，结果是位图，不能等同可编辑文字层。[官方产品](https://api.fanyi.baidu.com/product/23) |

腾讯旧 TMT 不宜新接：官方公告自 2026-10-01 停发新免费包并逐步降 QPS，2027-10-01 涉及接口停用。这不涉及上文独立的表格 OCR 产品，不能混为一谈。[当前公告](https://cloud.tencent.cn/announce/detail/2448)

### 6.2 完全离线翻译

| 方案 | 核实的文件大小与许可 | 工程与体验取舍 |
| --- | --- | --- |
| OPUS-MT/Marian INT8 | 官方 en→zh、zh→en 原始权重各约 312 MB；前者 Apache-2.0，后者 CC-BY-4.0；INT8 百 MB/方向是待生成预算 | 适合极小离线语言包、普通界面/短句；语义、术语和长段落质量需盲测，双向需两包。[en→zh](https://huggingface.co/Helsinki-NLP/opus-mt-en-zh/tree/main)、[zh→en](https://huggingface.co/Helsinki-NLP/opus-mt-zh-en/tree/main) |
| Hy-MT2-1.8B Q4_K_M | 官方 GGUF 约 1.13 GB；Apache-2.0，33 语种 | 离线均衡主要候选；支持术语、上下文及格式提示。需 llama.cpp 等原生运行时，CPU 冷热延迟和峰值内存待验。[模型卡](https://huggingface.co/tencent/Hy-MT2-1.8B)、[GGUF 文件](https://huggingface.co/tencent/Hy-MT2-1.8B-GGUF/tree/main) |
| Index-Translate-2B Q4_K_M | 官方 GGUF 约 1.31 GB；Apache-2.0，150 语种 | 多语种与结构化内容主要候选；纯文本不需要视觉编码器，不必多下 362 MB 的 Q8 mmproj。[模型卡](https://huggingface.co/IndexTeam/Index-Translate-2B)、[GGUF 文件](https://huggingface.co/IndexTeam/Index-Translate-2B-GGUF/tree/main) |
| Hy-MT2 极限量化 | 1.25Bit 文件约 462 MB，宣传为 440 MB | 体积吸引人，但依赖 STQ 特殊 kernel，关联 PR 当前仍 Open；主要优化证据为 Apple M4 ARM NEON，不能视为普通 Windows x64 包开箱可用。[模型说明](https://huggingface.co/tencent/Hy-MT2-1.8B-1.25Bit-GGUF)、[STQ PR](https://github.com/ggml-org/llama.cpp/pull/22836) |
| Argos Translate | 库为 MIT，完整 Python 栈另计 | 可研究模型/原型，不建议客户端直接携带整套 Python；底层原生 CTranslate2 可另做集成。[项目](https://github.com/argosopentech/argos-translate) |
| TranslateGemma 4B | 原始权重约 8.60 GB，Gemma 条款，需同意模型使用条款 | 当前不优先；量化后仍要按实际产物、速度和质量比较，不能拿原始大小代替所有量化版本。[模型卡](https://huggingface.co/google/translategemma-4b-it) |
| NLLB-200 distilled 600M | CC-BY-NC-4.0，官方模型卡定位研究 | 不进入默认生产分发 shortlist，不能因多语种就忽略权重条件。[模型卡](https://huggingface.co/facebook/nllb-200-distilled-600M) |

CTranslate2 是 MIT 的原生 C++ 引擎，支持 Windows x64、Marian、CPU INT8，可通过 FFI/worker 避免让用户安装 Python，但会增加一套运行时。已有 ORT 不代表已解决翻译的 tokenizer、encoder/decoder、cache 与生成循环。[原生安装](https://opennmt.net/CTranslate2/installation.html)、[Marian 转换](https://opennmt.net/CTranslate2/guides/marian.html)、[硬件支持](https://opennmt.net/CTranslate2/hardware_support.html)。同样，GGUF 不由当前 ORT 直接执行，需另一个原生推理组件。

## 7. 面向最终用户的四个组合

以下为选型配置，模型大小不含现有 OCR、共享 ORT、应用与附加运行时，也不等于运行内存。

| 组合 | 本地模型增量量级 | 使用体验 | 建议 |
| --- | --- | --- | --- |
| A 体积优先 | 表格约 8 MB；公式可选约 179 MB；翻译走在线 | 首装轻，日常表格本地，公式能力较窄，离线不能翻译 | 小磁盘或低频用户 |
| B 日常均衡 | 表格约 8 MB；公式可选约 260–319 MB；翻译默认在线，离线另选 | 日常能力本地完成，难题手动增强，默认包负担可控 | **推荐产品默认路线** |
| C 完全离线 | B + 1.13–1.31 GB 翻译，合计约 1.4–1.7 GB；或以待量化的小 MT 包压低体积 | 无日常联网依赖；首次下载、冷启动、内存成本增加 | 作为用户主动安装的离线能力包 |
| D 云端/已有模型服务优先 | 除本地 OCR/表格外几乎无模型增量 | 可接强模型，需网络/Key/费用管理；整图模式涉及图片上传 | 已有 API 或内网模型用户的高级配置 |

B 的“默认”指产品流程，不是默认下载所有模型。用户第一次选公式时再说明下载量并安装；不使用该能力的人无需承担。离线翻译只装选中的一个模型，不把两个 A/B 候选都变成正式依赖。

## 8. 决定体验的交互与输出合同

1. **结果先可读、可核对，再复制。** 公式同时呈现原图、渲染预览与 LaTeX；识别错误处能直接改。MathML、Word 原生可编辑公式另外验收，不能用“粘贴了一串 LaTeX”冒充完成。
2. **表格可做小修正。** 行列/合并格预览、单元格编辑、合并/拆分、区域复制，比不断换模型重识别更省时间。复制到 Excel/WPS 要实测，不仅检查 HTML 字符串。
3. **译文装不下有退路。** 保留原文对照、单段重翻与术语校正；复杂背景或译文溢出时提供侧栏阅读，不把字号一直缩到不可读。
4. **结构由本机持有。** 公式、代码、URL、数字和变量先保护；paragraph_id/cell_id 稳定，只翻译文字叶节点，校验返回 ID 和占位符。表格行列、跨度不交给翻译模型重新生成。
5. **按需加载、及时释放。** 识别与翻译任务后台执行，取消/重选不安装旧结果；普通截图不启动重模型。离线推理适合独立 worker 或受控会话，空闲释放，不让托盘常驻占用模型内存。
6. **服务切换可理解。** 初次说明本地、只传文字、上传选区图三种发送范围；本地失败保留结果，用户明确选增强后才转云端。缓存键包含服务/模型、方向、术语版本和原文。
7. **同主题、少干扰。** 下载与识别状态放在既有面板，跟随主题；不把独立模型设置、HTTP 错误和推理参数摆进日常主流程。

## 9. 定案前最小 A/B

本次调研不下载/运行模型。后续验证按候选闸门逐步进行，不需要一口气部署所有方案。

| 阶段 | 范围 | 通过后才做什么 |
| --- | --- | --- |
| 现有表格基础 | 30 张：有线6、无线6、合并格6、多行/空格6、深色/高DPI/小字6；包括现有失败样本 | 只把失败样本交给 UNet/LORE、SLANeXt 或一款云 API 对照 |
| 公式集成闸门 | plus-S 导出/加载与原生输出一致性；TexTeller INT8 解码与预处理一致性；连续调用、取消与截断 | 再比较同一批60张：打印15、多行/矩阵15、中文混排10、手写10、深色/低清10；Rapid 作轻量基线，Mathpix 作云对照 |
| 离线翻译 | Index 云、Hy 1.8B Q4、Index 2B Q4、OPUS INT8；80–100 个公开或自造片段 | 再决定最终只保留哪个离线包；未必要增加更多服务商 |

统一记录：冷启动与热运行 p50/p95、首次可读结果与完整完成时间、峰值工作集、空闲释放、下载/磁盘体积、失败后的可恢复性。OCR、推理、网络、排版分开计时。

质量记录：公式整式正确率、可渲染率及人工改到可复制的耗时；表格结构完全正确率、文字错误率、数字字段完全正确率和真实粘贴成功率；翻译盲评漏译/增译、术语、上下文、公式/代码保持。**最终以“用户拿到可用结果的时间与修正成本”定胜负。**

本次产物仅为选型报告，没有修改功能、路线、依赖清单或发布配置，也没有运行 cargo。所有速度和效果判断中未做本机实测的部分均保持待验。
