# 表格识别升级：方案比较与实施切片（2026-10-12）

方案对照采用参考实现；零结构模型后端与显式调用的独立 SLANet_plus 管线均已接入主目录。2026-10-16 按用户要求推进独立结构识别，运行时按需下载；验证与剩余范围见末尾。

## 结论先行

1. **推荐“零模型网格优先 + 可选结构模型”的混合路线**，不直接照搬截图Pro 的 SLANet_plus 管线：
   - 有线表（Excel、带边框网页表、深色主题网格）：像素**线检测**直接得到行列网格，缺失的内部线段推出合并单元格；
   - 只有横线／无线表（多数网页表、财报）：用已有 OCR 的字素级几何做**列留白聚类**，横线存在时用作行边界；
   - 证据不足或复杂表头：可选下载 **SLANet_plus（6.8–7.8 MB，Apache-2.0，已有 ONNX）** 作为第二候选，按一致性打分择优，不硬路由。
2. 理由：qk-ocr-lite 八轮调优（§1–§8）大部分精力都在**用真实网格线去纠正 SLANet 的几何漂移**（488 输入下密集列只剩二十多像素、边界偏 16–20 px、幽灵行列、501 token 截断）。对截图这种无透视、线条清晰的输入，线条本身就是更准、更快、无下载的结构来源；SLANet 的独有价值只在无线表和跨格表头。
3. 输出统一成一个 `TableGrid`（行列 + span + 文字 + 框），一次写入剪贴板 **CF_UNICODETEXT(TSV) + CF_HTML(含 rowspan/colspan)**：粘到 Excel／Word 保留合并格，粘到纯文本处仍是 TSV。截图Pro 的“复制 HTML”只是把 HTML 源码当文本写入（`Clipboard.SetText`），粘到 Excel 不会成表，不跟。
4. 识别**复用已有 OCR 结果**（RustShot 已有词框 + 字素采样），线检测和聚类只是毫秒级像素／几何运算；只有走模型时才多一次约 488² 的推理。
5. 评测直接复用 qk-ocr-lite 的 14 张夹具；它们的 `table-fixtures.html` 含完整单元格文字和 span，可生成**完整真值**，用 GridCell-F1 / 结构 TEDS-lite / 文本 CER 三件套，不用“结构 exact”做唯一门槛。

## 1. qk-ocr-lite 表格管线（截图Pro 实际所用）

**流程**（`OnnxTableStructureEngine.cs`、`TableStructureComposer.cs` 约 2000 行、`TableCellTextRefinement.cs`、`TableCellCropPreparer.cs`）：

| 阶段 | 做法 |
|---|---|
| 结构模型输入 | 长边等比缩到 **488**，左上对齐补零到 488×488；**BGR** 通道（导出自 OpenCV 管线），ImageNet mean/std 归一化 |
| token 解码 | 字符表读 ONNX metadata `character`，补 `sos`/`eos`、加 `<td></td>` 去 `<td>`；逐步 argmax，命中 EOS 停止；序列上限 **501 token**，撞上限补闭合标签并给 warning（§5.2） |
| 单元格框 | 每个开 cell token 取 8 个归一化坐标（四点），按 resize 比例映射回原图 |
| 置信度 | 全 token 均值被 `<tr>` 等填充 token 稀释（ICDAR 全部落在 0.977–1.000）；新增 `StructureMinConfidence`（最弱 cell token）才有区分度（AUC 0.757，PubTabNet 0.972） |
| 表格 OCR | 与普通 OCR 分开：按原分辨率检测（长边 736–3072，§1b），**逐字符框**（`ReturnSingleCharBox`）并对齐原行恢复空格，避免“1周三”“肱二头肌肱三头肌”整行跨格 |
| 网格校正 | ≥32 cell 的大表：同一逻辑边界两侧候选各取中位数再取中点；有线表：每条内部边界都在局部**脊线判据**（±3 px 两侧同向亮/暗差 ≥ t，t 按表格对比度缩放，深浅主题同一规则）下达到 65% 覆盖才整体吸附，否则整体放弃；尾部幽灵行列需“内外边界命中同一条强线”才折叠 |
| 文字分配 | 三段：重叠/间隙最近 cell → 同检测行伙伴仲裁歧义块 → 跨网格线的数字/字母碎片搬回邻格（修 `89877`→`8987|7`） |
| 复识别 | 对 ≤4 字、含中文、只剩单字或含 `]`/`1` 噪声的 cell 按逻辑边界内缩 3 px 裁剪，rec-only 复识别；上限 min(32, 20% cell)，置信 ≥0.8 且只接受补全/去噪 |
| 输出 | HTML（thead/tbody、span、多行 `<br>`）、TSV、cells（行列范围、文字、置信度、四角点）、warnings |

**暴露的问题与已知失败**（都是 SLANet 固有能力边界，后处理只能部分补救）：

- 密集列：20+ 列压到 488 后每列约 24 px，token 或边界一漂就分错列，事后无法恢复不存在的列。
- 结构边界偏移 16–20 px，超出网格吸附 ±10% 搜索半径（table-12 带留白版即失败，换成“只截表格框”才过）。
- 正文行被误判 `colspan`；`with-span` 样本 exact 仅 0.17（`no-span` 0.48）。
- 501 token 截断：约 400 cell 以上的大表只出前半张（ICDAR 5 张，吃掉 67% 未分配块）。
- DB 检测在网格线附近把数字串切开、把网格线识别成 `1`/`]`、短中文只剩“周”；检测分辨率对此**非单调**（736 过、928 败）。
- 线判据曾用绝对灰度：深色主题“全是墨”静默放弃、斑马纹抢走横线、无线表把文字列当竖线（§8 已改脊线）。

**历史评测数据**（small + CPU，来自当时另行准备的数据集；目录由运行者指定为 `<table-dataset>/reports/`，不依赖固定本机路径）：

| 集合 | 结构 exact | GridCellF1 | 结构 TEDS | 文本 exact | CER | 耗时 均值/P95 |
|---|---|---|---|---|---|---|
| PubTabNet 示例 20 | 0.90 | 0.991 | 0.990 | 0.55 | 0.234 | ~0.5 s / ~0.9 s |
| ICDAR 2013 167（稀疏真值） | 0.34 | 0.942 | 0.928 | 0.68 | 0.287 | ~0.8–1.1 s / 2.2–3 s |
| ICDAR 2013（`--densify-truth`） | 0.65 | 0.970 | 0.962 | 0.70 | 0.287 | 同上 |
| 自制夹具 14（small+tiny） | 28/28 通过（行列数 + contains/notContains） | | | | | |

教训：ICDAR 真值省略空占位格，44% 的“多 cell”是口径问题（§7），任何“空格合并”规则都会伤真值；结构 exact 随 cell 数塌陷而 GridCellF1 几乎不变，验收要看 F1/TEDS/CER 三件套。qk 自己的调研（`docs/research/20260826_表格结构识别成熟算法与RapidAI借鉴.md`）同集对照：RapidAI 有线 U-Net 单独 ICDAR 23.95%、LoRE 28.14%、分类器硬路由反而回归到 29.34%；SLANet + 两候选的 oracle 上限 45.51%——**候选互补明显，硬路由有害**。

## 2. 候选方案对比

| 方案 | 大小 / 许可 | ORT(ort crate, CPU) | 推理量级 | 截图场景适配 |
|---|---|---|---|---|
| **线检测网格**（脊线 + 覆盖率 + 交点/线段，零模型） | 0 / 自写 | 不需要 | 1500×1000 两遍扫描，估计 <10 ms（未测） | Excel/有线网页表/深色网格/斑马纹+网格：最强，列数无上限，合并格由缺失线段精确得到；虚线、只有横线、无线表：不适用 |
| **OCR 列留白聚类**（现启发式增强） | 0 | 不需要 | 毫秒级（未测） | 无线网页表、财报数字表、键值表：好；多层合并表头、跨格长文本、列内大量空格：弱 |
| **SLANet_plus**（PaddleOCR / RapidTable ONNX） | ONNX 6.8 MB（RapidTable 标注）；qk 实际 `slanet-plus.onnx` 7,758,305 bytes，SHA `d57a942a…514b`；Apache-2.0 | qk 用 .NET ORT 1.27 + RapidOcrNet 跑通；RustShot ORT 1.24.4/API24 加载与 metadata 读取**未核实**；需双输出 runner | 官方 CPU 高性能模式 41.8 ms（纯推理）；qk 整链含 OCR 0.5–1.1 s | 无线表与跨格表头是其长处；密集列/大表/边界漂移见上；Paddle 内部高难集精度 63.69 |
| SLANeXt_wired / wireless + PP-LCNet 表格分类（PP-StructureV3 / table_recognition_v2） | SLANeXt 各 351 MB；分类 6.6 MB；v2 还需 RT-DETR-L cell 检测各 124 MB；Apache-2.0 | ONNX 导出与 ORT 兼容**未核实** | SLANeXt CPU 501.66 ms（纯推理） | 精度 69.65（+6 点）但体积约 950 MB、CPU 慢；不适合本地按需下载，仅作离线对照 |
| RapidTable `unitable` | ~500 MB，PyTorch（无 ONNX） | 不可用 | CPU ~6 s | 不考虑 |
| TableFormer（docling） | HF 仓库标 apache-2.0 / cdla-permissive-2.0，具体归属与权重大小**未核实**；PyTorch | 官方 ONNX **未核实** | 未核实 | 论文 TEDS 93.6；体积与依赖不符合轻量约束，不考虑 |
| RapidAI U-Net 线分割 / LoRE 无线 | 8.3 MB / 约 88 MB，Apache-2.0（qk 调研核实） | ONNX 可用，需移植形态学/CenterNet 解码 | 1024² 输入 | 截图里线条已清晰，学到的线分割收益小（qk 同集仅补中 2 张）；LoRE 太大 |

来源：PaddleOCR [表格结构模型表](https://github.com/PaddlePaddle/PaddleOCR/blob/main/docs/version3.x/module_usage/table_structure_recognition.md)、[表格分类](https://github.com/PaddlePaddle/PaddleOCR/blob/main/docs/version3.x/module_usage/table_classification.md)、[RapidTable](https://github.com/RapidAI/RapidTable)（v3.0.0，模型列表与大小）、[docling-models](https://huggingface.co/ds4sd/docling-models)；U-Net/LoRE/TATR/SEMv2 的数字与链接见 qk 调研文档。PubTabNet/ICDAR 是文档 PDF 分布，与截图分布不同，上表“截图适配”一列是按机制推断，需 §4 评测确认。

**客观判断**：截图里 Excel 与带边框表占大头，线检测在这类图上理论上严格优于 SLANet（像素级边界、无 token 上限、无 488 降采样）；无线表上 SLANet 与列聚类各有胜负（模型能出表头 span，聚类在规则数字表上更稳且不会把正文误判为 colspan）。所以 SLANet 应是“候选之一”，是否默认启用由评测决定。

## 3. 推荐架构

```
OCR 结果(已有: 词框 + 字素采样) ─┐
截图 BGRA ── 线检测 ─→ 分类 wired / ruled(只有横线) / none
                         │
   wired  → 线网格 + 缺失线段=合并格 ──────────────┐
   ruled  → 横线定行 + 留白聚类定列 ───────────────┤→ 文字分配(字素级) → 候选校验/打分 → TableGrid
   none   → 行聚类 + 留白聚类 + 表头 span 推断 ────┤                         │
   (可选) SLANet_plus 候选（模型已下载且上面不确定）┘                  TSV / CF_HTML / Markdown
```

- **TableGrid 不变量**（所有候选都要过）：行列边界单调；每个基础格恰被一个矩形 cell 覆盖；span 连续；不丢已匹配文字。不过校验的候选直接淘汰。
- **线检测**：移植 qk §8 脊线判据（k=3，strong/light 两档 t 按 p0.5–p99.5 对比度缩放，对比度 <24 放弃）；但不是“校正模型边界”，而是独立找全图横/竖线段 → 合并共线段 → 交点成网格 → 每条基础边界逐段判有无线，无线段的相邻基础格合并。粗边框（>5 px）按尺度自适应 k；虚线（覆盖率 <0.65）降级为 ruled/none。
- **列聚类**：把每行字素区间投影到 x 轴，跨多行持续为空的竖带即列分隔；表头行中横跨多个列分隔的文字块推为 colspan；行由字素中心 y 聚类或横线确定。沿用现启发式的保守门槛，但放宽“每行列数相同、无空格、必须有数字列”，空格允许存在。
- **文字分配**：拆到字素级（用 `GraphemeSampling` 带宽，缺失时退到词框），照搬 qk §2 三段式和边界碎片修复；多行 cell 保留换行（HTML `<br>`，TSV 加引号内换行）。
- **复识别**（后续切片）：短中文单字/含 `]`、`1` 噪声的 cell，按网格内缩裁剪走已有 rec-only 路线，同 qk 上限与仲裁。
- **SLANet_plus（可选）**：模型按需下载，复用 `ppocr/assets` 的 bytes/SHA 校验与 Bitiful 镜像（新增一个 table 资产，镜像上传由发布端做），缓存 `%LOCALAPPDATA%/RustShot/ocr`；同会话复用 session，与 OCR 同一线程预算和取消令牌。只在 none/ruled 置信度低或用户选“模型优先”时运行；结果作为候选与零模型结果打分择优（证据：线覆盖、留白切穿文字数、未分配字素数、`StructureMinConfidence`）。下载失败/取消/截断 → 用零模型结果并在状态行说明。
- **失败与回退**：任何路径失败都不影响普通复制；全部候选不合格时保留现有 `OcrTableCopy` 结果，再不行则按钮禁用并提示（现行为）。不自动重排普通正文。
- **输出**：
  - 剪贴板一次写两种格式：TSV（`CF_UNICODETEXT`）+ `HTML Format`（CF_HTML，含 `StartHTML/EndHTML/StartFragment/EndFragment` 字节偏移头，UTF-8）。前导零、长数字 cell 加 `style="mso-number-format:'\@'"`，保持现有“保留零、前导零”承诺。Excel/Word 对两种格式的优先级与 mso 样式效果**未实测**。
  - Markdown 作为菜单次要项（span 展开为空格，注明丢失合并信息）。
  - 不做 xlsx 导出（截图Pro 走 HostBridge 生成 xlsx 打开 Excel；CF_HTML 粘贴已覆盖主要用途）。
- **入口衔接**：
  - 截图 OCR 面板“表格”视图：预览改为按 span 渲染的网格（替代 `ocr_panel::table_rows` 的 TSV 拆分），底部显示“N 行 × M 列，来源：线条/对齐/模型”；“复制表格”写多格式。
  - 贴图识别菜单“复制表格”同一后端；选字浮栏不新增入口（表格是整块结果，不按选区）。
  - “热键直达组合”（框选→表格→复制）见 [capture-quick-actions](../references/capture-quick-actions-2026-10-12.md)，复用同一 `TableGrid` 计算。
  - 表格计算在 OCR 结果安装时同步缓存（现 `TableCopy::new` 的做法），线检测需要截图像素，故缓存键加图像来源；SLANet 候选异步，完成后按来源/代次校验再替换。

## 4. 评测方案

- **夹具**：复制 qk 的 14 张 PNG（共约 0.5 MB，自生成无版权问题）到 `fixtures/table/images/`；用 `table-fixtures.html` 的源表生成**完整真值** `fixtures/table/ground-truth.json`（每 cell：行列范围、文字），保留 qk 的 tags（wired/wireless/dark/merged/dense/striped/dashed）与 contains/notContains 断言。生成用一次性脚本，脚本不入库，真值人工核对。
- **指标**（纯 Rust，移植 qk `TableStructureMetrics`）：GridCell P/R/F1（cell 键多重集）、GridCellText F1（NFKC+小写+去空白相等）、结构 TEDS-lite（Zhang-Shasha，`table>tr>td(span)`，不含文字）、CER、按 tag 分桶。不报“结构 exact”作门槛。
- **两级运行**：
  1. `cargo test --lib table_`：不需要模型——合成位图测线检测（黑白/深色/斑马/虚线/实心色带/低对比/粗边框/合并格）、合成字素框测聚类与分配、编码测试（TSV 引号、CF_HTML 偏移、span HTML、Markdown）、指标手算值（2×2 缺一格 = 1−1/7 等）。
  2. 样本评测：`#[ignore]` 测试或 CLI 子命令跑 14 张夹具（需本机已下载 OCR 模型；SLANet 可选），输出每张三件套 + 来源路径（线条/对齐/模型）+ 耗时，并与 SLANet 候选并排。可选扩展到另行准备的 PubTabNet 20 / ICDAR 167，运行者以参数指定 `<table-dataset>`；这些扩展数据不属于默认测试依赖，ICDAR 必须同时报 densify 口径。
- **准入门槛**：夹具 wired/dark/striped 桶 GridCellF1 = 1 且 contains/notContains 全过；wireless 桶不低于 SLANet；默认是否启用 SLANet 由“混合 vs 纯零模型”的 wireless/merged 桶差值决定（建议 ≥ 3 个百分点才默认开）。

## 5. 实现切片

依赖：S1 先落类型（半天量），S2/S3/S4 可并行；S5 在 S4 有数后决定是否推进；S6 由主会话统一接入。

| 切片 | 允许新建/修改 | 交付 | 验证 |
|---|---|---|---|
| **S1 表格模型与输出编码** | 新建 `src/table_grid.rs`、`src/table_export.rs`、`src/table_export_tests.rs`、`src/table_clipboard.rs` | `TableGrid`/`TableCell`（span、文字行、框、置信度、来源）、不变量校验；TSV（引号/换行规则与现行一致）、span HTML、CF_HTML 字节头、Markdown；Win32 一次 Open 写 `CF_UNICODETEXT` + `HTML Format` | `cargo test --lib table_export`；真实粘贴 Excel/Word 写“未验” |
| **S2 线检测网格** | 新建 `src/table_lines.rs`、`src/table_lines_tests.rs` | 输入 BGRA 区域，输出线段、wired/ruled/none 分类、基础网格与合并格；脊线判据与对比度缩放、粗线自适应 | `cargo test --lib table_lines`（合成位图 ≥8 类） |
| **S3 无线列聚类与文字分配** | 新建 `src/table_layout.rs`、`src/table_layout_tests.rs`；只读 `recognition*.rs` | 行聚类、留白列分隔、表头 colspan 推断；字素级文字分配三段式 + 边界碎片修复；产出 `TableGrid`，失败时回到现 `OcrTableCopy` | `cargo test --lib table_layout`；现 `recognition_table` 测试不回归 |
| **S4 评测夹具与指标** | 新建 `fixtures/table/`（14 PNG + 完整真值）、`src/table_metrics.rs`、`src/table_eval.rs`（`#[ignore]` 样本评测） | GridCell F1/TEDS-lite/CER/分桶；夹具评测输出表格 | `cargo test --lib table_metrics`；`cargo test --lib table_eval -- --ignored`（需本机 OCR 模型） |
| **S5 SLANet_plus 可选候选**（视 S4 结果） | 新建 `src/ppocr/table_structure.rs` 及其 tests、`dependencies/ppocr-table-cpu.json`；修改 `src/ppocr/runtime.rs`（多输出 run、读 custom metadata）、`src/ppocr/assets.rs`（table 资产）、`src/ppocr/mod.rs`（入口）——这三处当前无他人改动，开工前再看 `git diff` | 488/BGR 预处理、token/框解码、截断闭合、`StructureMinConfidence`；按需下载校验；候选打分接 S3 | `cargo test --lib table_structure`（假 runner 测解码）；真实模型样本评测并入 S4 |
| **S6 UI 接入（主会话）** | 修改 `src/recognition.rs`（`copy_table` 改走新后端）、`src/capture_ocr_panel.rs`（span 预览、来源说明）、`src/ocr_table_copy_ui.rs`（缓存 TableGrid）、`src/pin_view.rs` 菜单、`src/ocr_panel.rs` | 面板预览、复制多格式、贴图菜单、状态提示与回退文案（`tr!`） | `cargo test --lib ocr_table`；`cargo build` 后真实界面自测 |

**共享入口（只由主会话改）**：`src/lib.rs`（新模块声明）、`src/capture_view.rs`（`install_table_copy` 传图像来源、复制按钮）、`src/capture_canvas.rs`（行内按钮）、`src/preferences.rs`（可选“表格识别：自动／仅零模型／模型优先”）、`src/cli.rs`（若评测改为 CLI 子命令）、`Cargo.toml`（预计无新依赖）、`docs/behavior.md`。其中 `capture_view.rs`、`preferences.rs`、`lib.rs`、`capture_canvas.rs` 当前正被其它会话修改，S6 需等其落地后窄改。

## 未核实 / 风险

- SLANet_plus ONNX 在 ORT 1.24.4（ort `2.0.0-rc.13`, API24）的加载、custom metadata `character` 读取、双输出名；RapidTable README 同时写着“paddle2onnx 不支持 SLANet-plus”与提供 `slanet-plus.onnx`，以 qk 实测可用的文件为准。
- Excel/Word 对 CF_HTML 与 Unicode 文本的取用优先级、`mso-number-format` 在粘贴时是否生效、WPS 行为。
- RustShot ppocr 的字素采样精度是否足以替代 qk 的 `ReturnSingleCharBox` 逐字符框；RustShot 表格截图的 OCR 检测分辨率（qk 证明 736 vs 原分辨率会影响跨格）。
- 线检测、列聚类、SLANet 在 Rust 中的实际耗时与内存；以上耗时均为推断或他处数字。
- SLANeXt_wireless 数字、SLANeXt/RT-DETR 的 ONNX 可用性、TableFormer 权重许可与大小。
- 截图场景下零模型路线优于 SLANet 只是基于机制与 qk 调优记录的判断，需 S4 在 14 张夹具 + 若干真实截图（含 #382 类密集 Excel）上实测确认。

## 当前实现

`table_grid` 验证行列边界、矩形跨度与完整覆盖；`table_lines` 在深浅主题图像上检测脊线、合并线段并推断矩形合并格。`table_layout` 按“有线候选 → 旧可信矩阵 → 持续列留白”择取结果，再用字素／词框分配文字；来源几何不可信或超预算时返回 `None`。

PP-OCR 的完整裁行可超出词框联合范围，布局分别校验两种几何，不再要求完整裁行嵌在词框联合内；检测阅读轴允许约 6.8° 的小幅偏斜，明显斜行、竖排和 RTL 仍保留旧复制路线。线检测还排除了未连接正交轴的表头字形脊线，并按色带中心处理粗边框。

`table_export` 共用 TSV 引号规则并编码 span HTML、UTF-8 字节偏移 CF_HTML 与 Markdown；`table_clipboard` 一次打开剪贴板写入 Unicode TSV 和 HTML。现有截图／贴图复制按钮缓存同一 `TableGrid`，截图 OCR 面板显示行列、合并格和来源；快捷动作 `table-copy` 使用同一后端。新空间网格不可用，或旧矩阵转换为空间升序会改变原识别顺序时，缓存保留旧 TSV、清空富文本网格，按原识别顺序纯文本复制；倒序、RTL／180° 不因此失去表格入口。正常阅读顺序仍采用线检测的空列及合并格证据。

`fixtures/table` 是 14 张原创合成小图与完整真值，覆盖深浅、有线、无线、横线、密集网格、粗线、合并格和 2× DPI，不依赖维护者私有样本。`table_metrics` 提供单元格 F1、结构树距离和 CER；可选真实 OCR 评测并列比较旧矩阵与新后端，未找到候选记零分。

定向验证通过：线检测 11 项、布局 12 项、输出编码 6 项、指标 4 项、界面缓存 9 项及原表格复制 83 项；14 张真值输入的结构与文字合同全部通过。本机已有 Tiny 模型的实际 OCR 评测使用生产缓存最终择优：新结果 10/14，旧结果 0/14；有线 8/8，合并格 3/3，平均 F1/TEDS-lite 为 0.714、CER 为 0.286。夹具较小且含旧规则不支持的场景，不代表自然截图准确率。

早期宽过滤 `table_` 另命中了既有 `existing_editable_scalar_and_utf16_contract_does_not_take_readonly_grapheme_path` 文字导航断言失败；该模块未改，后续用上述精确模块过滤验证，未声称全库通过。

4 张无线夹具仍无表格；已确认其中 `wireless-light` 有上游 OCR 的 180°/90° 方向误判，不能用表格结构推断修正文字，其余失败夹具继续留作后续评测。真实评测不下载依赖、不打开窗口；SLANet 候选暂未引入。

真实 Excel／Word／WPS 的粘贴格式选择、自然截图与混合 DPI 识别仍待验。合成几何结果不代表真实 OCR 准确率；暂未引入 SLANet 结构模型或新下载资产。

## 识别入口整合与自动／手动表格（2026-10-13）

- **自动检测可行**：线检测原在 OCR 结果安装时于界面线程重读选区像素再扫描，整屏 1920×1080／2560×1440／3840×2160 为 62／105／229ms。现竖线在转置副本上逐行扫描、按半径整行求掩码（可向量化），两轴各一线程，降到 14／25／59ms；并移到 OCR 后台任务与识别并行（`capture_view.rs` 识别 worker 克隆一份像素给线检测），界面线程只做几何分配（夹具 <0.3ms）。计时口径：`cargo test --lib table_lines_full_screen_budget -- --ignored --nocapture`。
- **自动**（`TableCopy::with_lines` → `table_layout::recognize_detected`）保持原保守规则，检测到表格只在面板“表格”页签加圆点，不自动跳页。
- **手动**（识别下拉“表格识别”、表格页“仍按表格整理” → `TableCopy::forced` → `recognize_forced`）：先走同一可信候选；不足时源数据逐行容错（方向、倾斜、词框不合格的行退回词框联合近似，`source_pieces_with(.., true)`），有线网格忽略表外文字，对齐聚类两行即可、正文允许跨列、无列留白时退化为单列。夹具：自动 10/14，手动 14/14（4 张无线夹具结构 F1=1，剩余误差为 OCR 把 “Qty” 识别成 “140”）。
- **可视化**：表格页在原图上描出单元格（`capture_ocr_panel.rs::table_overlay_quads`），悬停面板单元格加深原图对应格。对照页由 `cargo test --lib table_eval_dump_visual_results -- --ignored` 导出 `target/qa/table-vis/results.json` 生成，已发布 https://claude.ai/artifact/492tK37yyAKfLdqyMfWU3R 。
- **正文夹表格**（`recognize_region`）：整体不成表时，取有线网格范围内的行，或按行中心聚成视觉行后“连续 ≥3 行、每行有两段以上留白分隔”的行段，单独跑对齐与分配；单元格平均超过 16 字或多数格 ≥12 字视为双栏正文而放弃。`TableCopy` 记录来源行号（`region`），面板据此混排；手动表格识别也先试这一路。
- **独立表格直接打开表格页**；其余默认段落页（原“原文”页改名“逐行”排第二）。段落页“复制全文”把表格部分换成 TSV（`ocr_mixed_text`）。
- **边界与原表对不上**（用户截图：报告页叠了两套网格，线检测没拼出网格，退回对齐，列边界落在文字间隙中点）：对齐候选的列边界在留白区间内、外框在文字外侧 3 倍行高内吸附到检测到的竖线；行边界同理吸附横线（`snap_edges`，线段须与表格跨度重叠过半，吸附后须严格递增）。按 1.257 倍放大复现，列边界从间隙中点改为真实竖线。
- **双线**：线检测把双线之间的窄条当成一行（且因没有竖线被判成整行合并格）。`collapse_thin` 把没有文字、不到正常行列 1/4 且不超过文字高度的窄行窄列并入相邻边界（外侧直接去掉），只完全落在窄条里的合并格允许随之去掉；真实空行空列保留。单图口径：`RUSTSHOT_TABLE_IMAGE=<png> cargo test --lib table_eval_single_image -- --ignored --nocapture`。
- **正文夹表格的首尾杂行**（用户截图：卡片标题行与图下状态行各有两段留白，被并进表格，或使整段对齐失败而段落页退回逐行）：行段候选同时给出去掉首行／末行／首末两行的版本；成表后首尾非空单元格不到各行中位数 2/3 的行剔除重算（最多三轮，剔除的行仍按正文显示）。用用户截图裁出的选区复现，得到正确的 4×8 且标题、状态行留在正文。
- **自动识别常失败的根因**（用户实测“识别不出表格”）：单个数字等短单元格（如“8”）裁行高于宽，检测给出竖直阅读轴，严格源数据校验视为竖排而整表放弃；无线表、横线表、正文夹表格因此几乎都失败。现在 ≤3 字且框不比高宽的短行按横向词框近似（宽框的竖直／倾斜轴仍拒绝，原竖排／倾斜测试不变）。另在线检测里把相隔 ≤6px、长度重合 80% 的平行双线并为一条（`merge_double`），网页双线表恢复为有线网格。用微软雅黑生成的 8 张真实风格表（Excel 1×／1.5×／2× 深色、网页双线、无线、表头底色、正文夹有线／无线）加用户截图选区全部自动成表；夹具自动 11/14（原 10/14）。图片生成脚本不入库，`table_eval_single_image` 逐张检查。
- 未验：真实截图界面的下拉、页签圆点、混排、网格描边与悬停；自然截图（非夹具）的手动与正文夹表误判率。

## 短键长值表格的换行（2026-10-16）

- 用户截图中，前几行短值成表，后面的换行值退回正文。离线重跑确认 OCR 已读到全部文字；旧结构结果只有表头和前四行。原因是局部候选遇右列单段续行就结束，列留白支持又按视觉行数计算，长值还会触发双栏正文过滤。
- `table_layout.rs` 增加严格的两列键值续行归属：至少三组稳定列锚点、左侧短标签，右列续行起点一致且贴近前行时合并为逻辑行；真实横线阻止跨行合并。局部候选允许从首个值开始换行，明显段间距后的说明仍为正文；长值不再单独触发双栏正文过滤，格内换行与英文空格照常保留。
- `table_layout_tests.rs` 覆盖正文夹八行键值表、首行即换行、无线表多行值、仅横线表、真实空左格与对称双栏正文。增量 `cargo check` 已通过（既有未使用项警告保留）；定向测试与用户截图的新版离线结果待其它会话的 Cargo 构建结束后验证，真实界面未验。
- 此阶段只改 OCR 读字＋像素线条／对齐的结构规则；随后接入的独立 SLANet_plus 见下一节。

## 独立表格结构识别（2026-10-16）

- 用户明确要求加入独立模型能力。`ppocr/table.rs` 在后台依次取得原分辨率 OCR 与 SLANet_plus 结构，`table_structure.rs` 用 Rust 实现 488/BGR 预处理、模型内置字典、双输出解码、行列及 rowspan／colspan；`table_layout::fill_structure` 复用字素填字，保留英文空格和格内换行，拒绝切掉半行文字的结果。
- `dependencies/ppocr-table-cpu.json` 单独固定 7,758,305 bytes 模型及 SHA256；`ppocr/assets.rs` 首次显式任务按需下载，复用已校验的 ORT 与取消／代理／原子落盘，缓存到本机 OCR 目录。表格模型当前使用已核实的 ModelScope RapidTable v2.0.0 公开源；已有 OCR 运行库及许可证仍优先 Bitiful。未发布表格镜像，不写不存在的镜像地址。
- 截图识别下拉“表格识别”、表格页“用模型识别表格”、快捷动作 `table-copy`、贴图“复制表格”均接独立后台任务。普通 OCR 的自动表格候选保持无结构模型；显式任务沿用来源／内容代次／请求身份取消守卫，完成后才安装结构缓存。贴图只复用已运行过独立任务的当前缓存；模型结构不再被旧文字矩阵覆盖，复制失败保留结果供重试。
- 同一文字覆盖且有完整真实网格时保留像素线结构；规则严格覆盖更多来源行时保留完整结果并提示。下载、输出截断或低置信度失败时保留 OCR 与规则降级结果；取消不转普通错误。CLI 新增 `--table-file <image> [--ocr-profile tiny|small]`，JSON 包含文字、单元格跨度、TSV／HTML、来源、表内行号和降级说明。
- 已补前处理、解码、跨度、覆盖择优、取消、缓存与 CLI 定向回归。接入阶段 `cargo check` 通过；最后的缓存身份校验、定向测试与首次下载／真实模型离线验证受其它会话持续构建占用，尚未运行。测试产物早于本次源码，未复用冒充已验；当前运行版未更新，真实截图／贴图界面、Excel／WPS 粘贴及混合 DPI 未验。
