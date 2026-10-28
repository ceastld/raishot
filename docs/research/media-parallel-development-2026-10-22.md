# 媒体优化并行开发协调

用户已授权设定持续目标、创建五个独立 Codex 开发对话，并允许对话为本项目互相发消息、向总协调对话报告，要求开发与编译不冲突。总协调为当前“编写功能优化计划”对话；功能任务按这五个对话分派，共享构建时段同时与同仓库既有长图对话协调。

总任务表：[媒体查看、录屏与工具优化](../references/media-viewers-recording-tools-plan-2026-10-22.md)。先完成 M0 有声播放，再沿原计划继续。本文件是本批文件所有权与编译约定的唯一补充记录。

## 协作规则

- 五个开发对话均使用当前主工作区，先检查 git status/diff，保留所有既有改动，不新建源码副本或 worktree，不 commit/push。
- **总协调统一安排 cargo、构建、测试与本地运行版，可把完整编译时段交给同仓库其它负责人；持有时段的一个会话执行。五个开发对话不自行启动 cargo，不创建编译目录，不启动产品/QA窗口。** 可以对自己文件运行 rustfmt。
- 总协调编译前检查 cargo/rustc 进程；有其它编译时先做独立工作。需要验证的对话报告文件/API和建议过滤词，由总协调安排；不要启动等待锁的 cargo。
- 用户同时在 Claude 开发同一仓库。它的编译也占用主 target，不能仅凭 Codex 对话交接判断空闲；不终止其进程，不因此冻结任何方向。检查与启动之间若撞锁，仅取消本会话排队的 cargo，继续源码集成，下一自然交付点再验证。
- 交付一批可集成代码后，集中报告接口、验证建议和未验范围，然后继续开发。编译不锁源码，也不作为继续下一项的门槛；验证只对该次构建读到的代码记结果，后续变化进入下一批。
- 共享入口 `lib.rs`、`Cargo.toml`/`Cargo.lock`、二进制、`file_launch_command.rs`、`main_page.rs`、`settings_view.rs`、`main_history.rs`、托盘/工具栏注册、公开说明和本机交接由总协调独占。开发对话只给接线说明，不直接编辑。
- 每个开发对话可直接发送必要接口、依赖与合并消息；避免逐条进度广播和互相等待。只有同一个共享hunk需要双写时短暂交接，普通独立模块不锁。
- 新能力先内部/实验；人工验收与代码/自动验证分别记录。一批交付不是整个方向结束，不等待总协调逐项派单。

## 自驱动责任制

媒体相关共享入口接线及该批 Dev 编译、原生验收已由用户交给视频核心与音频负责人接管；其它方向继续各自源码，沿用主工作区 target。用户另准本轮媒体验证与 ClipHost 的独立输出目录并行编译；RustShot 同一 target 内仍由一个负责人串行执行。

用户已明确要求各对话自己连续推进，取代“做一批、等一条消息”的调度。五个对话各自持有方向持续目标，自行选择总计划内的下一项、作技术取舍、补必要模块/样本/测试，并直接与其它负责人对齐接口；只按可集成批次回报，不逐个函数请求派单。

| 长期负责人 | 责任范围 | 自主文件范围（含相同前缀的新模块） |
| --- | --- | --- |
| 视频核心与音频 | V01–V04/V09/V10/V14、A01–A08 | Player/解码循环、preview_audio、preview_audio_*、media_playback_*、video_decode_*、video_seek_*、video_probe_*；MF读取区 |
| 视频窗口与工作流 | V05–V08/V11/V12，后继E01/E06 | video_view*、video_controls*、video_shortcuts*、video_frame_export*、video_file_queue*、video_open*、video_gif*、video_export*；gif_export.rs兼容扩展 |
| 图片查看与格式 | I01–I12、F01–F12 | image_viewer*、image_collection*、image_decode*、image_metadata*、image_animation*、image_format*、image_webp*、image_tiff*、image_color* |
| 录屏编码与质量 | R01–R14 | recording_encoder*、recording_options、recording_quality*、recording_color*、recording_size*、recording_recovery*；MF编码区 |
| 图片工具与批处理 | T01–T07/T10 | image_tools*、image_tool*、image_split*、image_collage*、image_resize*、image_transform*、image_batch*、image_compress*、image_pdf*、image_compare* |

总协调负责共享入口、依赖、集成、编译排程和开放状态，不逐项批准负责人范围内的普通实现。MF同文件的读写双方直接交接相关hunk，避免双写；未列的跨区文件和依赖先对齐所有权。录屏负责人另持有 `recording.rs` 的Encoder Pending/resolve/创建与首样本事务，以及 `recording_output.rs` 的attempt暂存分配、失败清理与晋升区域，沿旧输出保护合同接R02。

后续接线授权：视频核心维护 `audio_capture.rs` 的设备解析与带设备选择启动；录屏负责人维护 `recording_view.rs` 的录前编码/音频选项、对应布局/字段与start/prepare调用，保留已接预览警告，并维护 `wav_track.rs` 的写入/收尾容量保护。图片负责人单写 `save_options.rs` 的Webp格式定义/解析及 `export.rs` 的无损编码分支/回读测试，工具负责人接自己的格式按钮；默认配置与正式保存入口暂不拓宽。

视频窗口方向另独占 `crates/gif-codec/src/palette.rs` 的 `Histogram::build_with_colors` 兼容扩展与必要回归，原 `build` 默认256色及既有录屏转换行为保持。CLI接线仍由总协调负责。

用户已进一步取消全量源码冻结：各负责人现在就可持续修改自己范围的已注册源码，cargo运行也不例外，不等待根释放源码时段。保持旧接口可用或双方完成兼容接线，避免先挂缺文件/缺方法引用。编译遇到并行中间态，只报具体错误并在自然稳定点验证，不扩大锁定、不反复重跑。某个共享hunk短暂合并不能阻塞整个方向；独立工作自行推进。

## 首批交付参照（当前所有权以上表为准）

| 对话 | 任务 | 独占允许修改 | 首批交付 |
| --- | --- | --- | --- |
| 视频核心与音频 | V01–V04、A01/A02，随后V09/V10/V14 | `recording_preview.rs` 的 Player/解码循环区域；`media_foundation.rs` 读取/解码区域；新 `media_audio*.rs`/`media_probe.rs`；必要时 `preview_audio.rs` | 通用文件内嵌音轨、可复用播放器，保留现有WAV分轨录屏回放 |
| 视频查看界面 | V05/V08/V11，随后V06/V12 | 新 `video_view.rs`、`video_controls.rs`、`video_shortcuts.rs`、`video_frame_export.rs` | 文件选择和原生播放窗口，当前帧/错误/进度/音量等完整状态 |
| 图片查看与格式 | I01–I04/I06、F01/F02，后续常用格式/动画 | 新 `image_viewer.rs`、`image_collection.rs`、`image_decode.rs`、`image_metadata.rs`；`export.rs` 仅解码相关区域 | 只读查看、目录翻图、复用缩放、读入预检/方向；不改现有image_editor |
| 录屏编码与质量 | R01/R03，后续R02/R08/R09 | 新 `recording_encoder_caps.rs`；`recording_options.rs`；`recording_quality_lab.rs` | 编码器能力/实际可协商信息、质量预设模型；首批不修改MF编码区域 |
| 图片分割与拼接工具 | T04/T05，随后T02/T03 | 新 `image_split.rs`、`image_collage.rs`、必要的 `image_tools.rs` | 有界处理、参数、预览/输出、取消；原图保护，先给内部入口接线 |

MF读取与编码按当前长期责任表分区，共享hunk由两负责人直接交接。视频界面使用独立模块；跨区文件在开始前对齐所有权。

## 第一批接口约定

先复用 `crate::recording_preview::Player`，保留现有 `open(path, tracks, levels)` 与录屏接口。为通用视频添加：

- `Player::open_file(path: PathBuf) -> Self`，异步加载且能报告线程/解码失败。
- 既有 `toggle`、`seek_ratio`、`set_rate`、`playing`、`position`、`size`、`error` 继续可用。
- `take_frame` 对同crate开放，返回现有 `(代次, BGRA Vec)`，宽高来自 `size`。
- 新增 `set_volume(&self, value: f32)`（0–1）和 `set_muted(&self, bool)`；声音不可用可读 `audio_error`。具体可变接收者或错误接口若调整，双方消息确认。
- 新视频入口：`video_view::run(path: Option<PathBuf>) -> crate::Result<()>`；`None` 时显示文件选择。新图片入口同名合同 `image_viewer::run(Option<PathBuf>)`。

总协调接 CLI/worker 的 `--view-video [path]`、`--view-image [path]` 与历史录屏入口。所有外部文件只读打开，不将其纳入录屏临时文件清理。后台声音有界读取、暂停/seek清空旧缓冲；首轮H.264/AAC可用后再扩格式。

显式查看参数用于内部验证；新历史播放按钮在人工验收前默认隐藏，仅本机 `RUSTSHOT_EXPERIMENTAL_MEDIA=1` 时显示。原历史主动作暂沿系统播放器，三入口验收通过后再调整正式默认。CLI沿现有文件编辑入口的等待窗口关闭语义，历史通过spawn启动不会阻塞。

## 当前安排

2026-10-22用户明确转入人工测试交付；停止自动新增功能，各方向只收口当前编辑并报告边界，之后仅处理测试发现的问题。
源码和未提交改动保留在共享工作区；最新范围以`docs/status.md`及各专题为准，不把旧批通过扩展到后继源码。
媒体集成负责人固定本轮Dev测试候选，只做交付必需的一致性与预览像素确认、更新运行副本；后继专项不再不断加入队列。总协调汇总进度及人工清单，未纳入当前构建的源码留待下一批，未改的绿色测试不重跑。

上一集成批已接帧PTS与像素共同交接、后台剪贴板准备/序号检查、缩略图、实际编码诊断、策略/变换/批处理，以及队列/时钟/GIF/内部工具窗；最后的保存提交前取消与缺失原路径资格也经check通过。批处理自定义单边限制尚未进头部预检，既有整图调整阶段仅在调用前后取消，产品化前补齐。

前批复用Claude生成的Dev测试程序，定向572过/1失败/19跳过，原生11过/5失败，原批计数与后继逐项修复分开记录，具体范围集中见总计划。媒体接管后的集成、原生回读、数字声音环回，以及后继图片／录屏验证均见相应专题；运行Dev只含已部署批次。各方向继续写，不扩大冻结。

| 技术工作项 | 公开资料入口 |
| --- | --- |
| 总体依赖与集成 | [媒体查看、录屏与工具计划](../references/media-viewers-recording-tools-plan-2026-10-22.md) |
| 视频核心与音频（V01–V04/V09/V10/V14、A01–A08） | [媒体音频处理](../references/media-audio-processing.md) |
| 视频查看界面（V05–V08/V11/V12、E01/E06） | [媒体查看任务与接口](../references/media-viewers-recording-tools-plan-2026-10-22.md) |
| 图片查看与格式（I01–I12、F01–F12） | [图片动画与解码预算](../references/image-animation-streams.md) |
| 录屏编码与质量（R01–R14） | [录屏输出生命周期](../references/recording-output-lifecycle-2026-10-11.md) |
| 图片分割、拼接与批处理（T01–T07/T10） | [图片工具工作流](../references/image-tools-workflow.md) |
| 长截图编辑与共享入口 | [长截图完成流程](../references/long-editor-redesign-2026-10-20.md) |
