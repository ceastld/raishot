# 下一批功能的历史实施方案（2026-10-12）

每个任务写明：依据、允许改的文件、交付、验证。快捷动作、零模型表格识别与实时窗口贴图已进入主目录实现，验证和剩余范围见对应专题末尾。通用规则以 `AGENTS.md` 为准（主 `target/` 增量编译、`cargo test --lib <过滤>`、同时只跑一个 cargo、窄改共享文件、文案 `tr!`、窗口跟随深色主题）。

**历史范围**：当时暂缓抠图与公式识别，优先实现快捷动作、表格识别和实时窗口贴图。以下切片保留技术取舍，不作为当前任务分配或发布要求；当前开发入口见[文档导航](../README.md)，迭代顺序见[路线](../roadmap.md)，许可见[许可说明](../license.md)。

**共享入口**：`src/lib.rs`、`src/preferences.rs`、`src/capture_view.rs`、`src/capture_canvas.rs`、`Cargo.toml`、工具栏注册常被多个会话同时修改。每个任务开工前先跑 `git diff <文件>`，只做窄改；新模块的 `mod` 行和挂点放到该任务的最后一步再加。

## 建议顺序

| 顺序 | 任务 | 依赖 | 可并行 |
| --- | --- | --- | --- |
| 1 | Q1 快捷动作模型与存储 | — | 与 T1、O1 并行 |
| 2 | T1 表格结果类型与输出编码 | — | 与 Q1、O1 并行 |
| 3 | O1 开源准备：仓库内整理 | — | 与功能任务并行（只碰文档、脚本、配置） |
| 4 | Q2 会话执行 + Q3 参数传递 | Q1 | Q2 与 Q3 可并行 |
| 5 | T0 表格评测集 → T2 线检测、T3 无线表列聚类 | T1 | T2 与 T3 可并行 |
| 6 | Q4 热键、托盘、CLI；Q6 完成提示 | Q2、Q3 | — |
| 7 | T4 表格评测；T6 表格界面接入 | T2、T3 | — |
| 8 | Q5 设置页 | Q1 | — |
| 9 | W1–W3 实时窗口贴图第一档 | — | 先等用户回答 W 的待定项 |
| 10 | T5 SLANet_plus 可选候选 | T4 数据显示有必要时才做 | — |
| 11 | O2 新建公开仓库与私有仓库 | O1 | 需要用户在场 |

## Q：截图后快捷组合操作

设计全文见 [capture-quick-actions](../references/capture-quick-actions-2026-10-12.md)，实现切片在第 6 节。一条快捷动作由四部分组成：采集源、是否先编辑、1–3 个固定步骤、历史策略。可以绑定热键、放进托盘子菜单，或用 `rustshot --action <id>` 触发。不做脚本引擎。

待定项暂按推荐默认实现，做成容易改的常量：
1. 动作失败时回到编辑栏并保留选区。
2. 只出文字的动作（OCR、表格、扫码）默认不写历史；出图的动作跟随全局历史设置。
3. 设置了“完成动作”后，✓、Enter、双击都跟随它；Ctrl+C 固定为复制图片。
4. 内置预设一律不绑热键。

| 任务 | 文件 | 交付 | 验证 |
| --- | --- | --- | --- |
| Q1 模型与存储 | 新建 `src/quick_action.rs`、`src/quick_action_store.rs`、`src/quick_action_tests.rs`；`lib.rs` 加 mod | `Source/Step/QuickRecipe`、`u32` 打包与解包、组合校验、内置预设表、CLI 名解析、`tr!` 标签；独立的 `quick_actions.ini`（只存与内置的差异，损坏行跳过） | `cargo test --lib quick_action` |
| Q2 会话执行 | 新建 `src/capture_quick_action.rs`、`src/quick_action_flow.rs`；`capture_view.rs` 加挂点（窄改） | 选区完成后立即执行步骤；失败回到编辑栏；替换现有四处硬编码为“复制”的完成调用点；按会话去重写历史 | `cargo test --lib quick_action_flow`；用 flicker-probe 自测有无遮罩闪烁 |
| Q3 参数传递 | `src/capture_standby.rs`、`src/ui.rs`、`src/gpui_capture.rs`、`src/bin/rustshot-gpui.rs`；新建 `src/last_region.rs` | 动作编码走待命进程通信里空着的字段和托盘消息的高位；冻结桌面后再读“上次区域” | `cargo test --lib capture_standby`、`last_region`；托盘通知到可框选约 200ms 不退化 |
| Q4 热键、托盘、CLI | `src/hotkey_registration.rs`、`src/ui.rs`、`src/tray.rs`、`src/cli.rs` | 热键 ID 区间重划（设计第 6 节），最多 16 条快捷动作热键；托盘子菜单；`--action` 转发 | `cargo test --lib hotkey_registration`、`quick_action` |
| Q5 设置页 | 新建 `src/settings_quick_actions.rs`；`settings_view.rs` 只接一行入口 | 列表、编辑区、冲突检查；“截图”页加“完成动作”下拉 | 纯校验复用 Q1 测试；真实界面写“未验” |
| Q6 完成提示 | 新建 `src/quick_toast.rs` | 截图进程退出后，由常驻进程用 GDI 画轻量提示（如“已复制 123 字”） | 窗口跟随深色主题；真实界面未验时如实写 |

最小可用版本是 Q1–Q4：已经能用热键直接“框选 → OCR → 复制”。

## T：表格识别升级

方案见 [table-recognition](table-recognition-2026-10-12.md)。结论：截图里的表格以“线检测（有线表）+ 列对齐聚类（无线表）”为主，不需要模型；SLANet_plus 只作为可选下载的交叉候选。复制时一次写两种剪贴板格式：TSV 和带合并单元格的 CF_HTML。注意截图Pro 的“复制 HTML”只是把 HTML 源码当纯文本写入，不要照搬。

| 任务 | 文件 | 交付 | 验证 |
| --- | --- | --- | --- |
| T1 类型与输出 | 新建 `src/table_grid.rs`、`src/table_export.rs`、`src/table_export_tests.rs`、`src/table_clipboard.rs` | `TableGrid`/`TableCell`（rowspan/colspan、文字、框、置信度、来源）；TSV（与现行规则一致）、HTML、CF_HTML（偏移按 UTF-8 字节计算）、Markdown；一次打开剪贴板写入两种格式 | `cargo test --lib table_export`；粘贴到 Excel/Word/WPS 的效果写“未验” |
| T0 评测集 | 使用 `fixtures/table/`（PNG、`ground-truth.json`、`README.md`） | 从仓库已有的 14 张原创合成表格和完整真值继续扩展；按截图场景补有线、只有横线、无线、网格、合并单元格、深色主题、2x DPI、局部表格等用例 | 单张不超过 200KB |
| T2 线检测 | 新建 `src/table_lines.rs`、`src/table_lines_tests.rs` | 灰度加自适应对比（兼容深色主题）；检测长线段，合并相近线；区分有线、只有横线、无线三种；从内部缺失的线段推出合并单元格；所有数量都有上限 | `cargo test --lib table_lines`，测试用代码合成的像素图 |
| T3 无线表聚类与文字分配 | 新建 `src/table_layout.rs`、`src/table_layout_tests.rs` | 行聚类、按列间留白分列、推断表头 colspan；字素级三段式文字分配；失败时回到现有 `OcrTableCopy` | `cargo test --lib table_layout`；现有 `recognition_table` 测试不回归 |
| T4 评测 | 新建 `src/table_metrics.rs`、`src/table_eval.rs`（`#[ignore]` 或单独命令） | 指标：单元格匹配 F1、结构树编辑距离得分（TEDS-lite）、字符错误率；跑 T0 的样本，比较线检测/聚类与现有启发式 | 纯指标做单元测试；样本评测需要本机 OCR 模型，可选 |
| T5 SLANet_plus（视 T4 结果） | 新建 `src/ppocr/table_structure.rs`、`dependencies/ppocr-table-cpu.json`；改 `src/ppocr/runtime.rs`、`assets.rs`、`mod.rs` | 488 BGR 预处理，token 与框解码，截断时补闭合；按需下载并校验；和 T2/T3 的结果比一致性择优 | 先确认模型能在 ORT 1.24.4 上加载；`cargo test --lib table_structure` 用假推理测解码 |
| T6 界面接入 | `capture_ocr_panel.rs`、`ocr_table_copy_ui.rs`、贴图菜单；共享入口窄改 | OCR 面板显示表格预览和“复制表格”；接入 Q 的 `table-copy` 步骤 | 相关过滤测试；真实界面未验时如实写 |

## W：实时窗口贴图（第一档：只看不点）

方案见 [window-pin-live](../references/window-pin-live-2026-10-12.md)。用 DWM 缩略图（`DwmRegisterThumbnail`），由系统直接合成，我们不逐帧拷贝，也不经过 GPUI；源窗口被遮挡时照样实时刷新。转为静态贴图、OCR、复制时才抓一帧，首版使用 WGC，失败沿用冻结像素并提示可能不是最新画面；省略可能阻塞源应用的 `PrintWindow` 回退。

**用户已确认**：输入转发实测收益低，首版只显示实时内容；性能优先，允许 WGC 只在输出时抓取一帧；退出和重启尽可能恢复，保存为静态工作区记录。Windows 后端采用 DWM，区域和状态模型独立；macOS 可另接 ScreenCaptureKit，本轮不实现 macOS。

| 任务 | 文件 | 交付 | 验证 |
| --- | --- | --- | --- |
| W1 模型 | 新建 `src/pin_live_model.rs`、`_tests.rs` | 区域换算（DWM 扩展边框、负坐标、多屏、源窗口 DPI 不感知）、`rcSource`/`rcDestination` 计算、坐标映射、状态机（实时、最小化、源窗口关闭、尺寸变化、已转静态）、与穿透互斥 | `cargo test --lib pin_live_model` |
| W2 DWM 封装与专项 | 新建 `src/pin_live_dwm.rs`、`src/pin_live_smoke.rs` | 先验证最大的未知点：缩略图与 GPUI 自绘内容的叠放顺序、与贴图透明度能否共存、DPI 不感知的源窗口 | 只用自有的源窗口和宿主窗口，深色主题，不弹白窗 |
| W3 贴图窗与入口 | 新建 `src/pin_live_view.rs`、`src/pin_live_snapshot.rs`；入口窄改 `capture_view.rs`、`capture_pin_output.rs` | 实时贴图窗、右键菜单、转为静态（复用现有图像贴图的打开函数） | 用 flicker-probe 自测；真实应用未验时如实写 |

## O：历史开源准备方案的后续

早期许可和双仓库发布结构已经调整，旧迁出清单及 submodule 方案不再执行。当前项目采用 MIT，公开仓库提供完整的源码、构建、默认测试和开发上下文；以[文档导航](../README.md)、[许可说明](../license.md)和[贡献指南](../../CONTRIBUTING.md)为准。

继续适用的边界只有：凭据不进入任何 Git 仓库；默认构建与测试不依赖维护者本机目录；未随仓库发布的历史实验材料不能作为已通过或可复现的证明。技术专题和原型随公开文档维护，个人配置与操作流水不作为开发前提。

## 其它参考

- 截图基础体验差距审计（未写完，结论和大部分对照表可用）：[capture-basics-gap](capture-basics-gap-2026-10-12.md)。
- 抠图（U2NetP/MODNet）、公式识别、AI 图片编辑、JS 工具栏：本轮不做。
