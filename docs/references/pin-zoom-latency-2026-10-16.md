# 贴图缩放时内容跟不上窗口（2026-10-16）

## 现象与原因

用户反馈：滚轮缩放贴图时，窗口先变了尺寸，内容过一会儿才跟上。

截图贴图总带 `SelectionFinishStyle`（效果全关也一样），底图走 `pin_presentation_preview::prepare_body_pixels`。计划（倍率、原点）一变，就要按新倍率在 CPU 上重新采样整块可见底图：`display_content` 对每个目标像素调用一次 `pin_color_sample::linear_pixel`（预乘双线性、f64）。

与此同时，窗口尺寸由 `pin_view::request_placement` 先通过 `SetWindowPos` 生效，新帧要等这一步采样完成才能 Present。截图贴图每帧都校验内容完整（`pin_content_drawn_size`），不允许用旧底图拉伸的占位帧，所以采样耗时就等于“窗口已变、内容未变”的时长。

## 改动

`pin_color_sample::linear_background` 做整块采样，`display_content` 与取色快照的 `background()` 都改用它：

- 每个目标列、行的源下标与小数权重只算一次（`source_center` 可按 x、y 分开）。
- 每个用到的源行只按 `pixel` 的同一优先级合成一次：后面的平面与光标覆盖前面的，任一覆盖平面缺字节即为错误，空隙用填充色。合成时保存 `通道 × alpha` 的 f64。
- 每像素四个采样点的累加顺序、零权重跳过、最终除法与取整都与 `linear_pixel` 相同，输出逐字节一致。`f64::round` 在基线目标上是库调用，改为截断后比较余数，对非负有限值与 `round` 逐位相同。
- 每 256K 目标像素一个线程，最多用一半逻辑核（不超过 16），按行带并行；小块单线程。
- 退化尺寸（源或目标宽高为 0）保持原逐像素路径。

## 验证

- 新增 `linear_background_matches_per_pixel_sampling_byte_for_byte`：随机三平面、光标覆盖、0／1／中间／255 透明度，放大、缩小、分数原点与非等比缩放，以及缺像素报错和补填充色，全部与逐像素路径逐字节一致。原有 `unscaled_background_*` 像素合同、`pin_color_sample` 12 项、`pin_export` 12 项、`pin_zoom` 13 项通过。
- `pin_presentation` 52 项通过。1 项失败为既有的 `dual_4k_preview_preflight_*` 旧四角预算断言，见 [首显专题](pin-first-show-latency-2026-10-12.md)。`capture_pin` 有 1 项输出原点断言失败，属几何问题，与采样无关，疑来自工作区其它会话的选区外观改动。
- 计时（`cargo test --lib linear_background_performance -- --ignored --nocapture`，1920×1080 源，32 逻辑核、机器另有约 30% 负载）：

| 目标 | 逐像素（旧） | 整块（新） |
|---|---|---|
| 0.75×（1440×810） | 88ms | 25ms |
| 1.25×（2400×1350） | 231ms | 32ms |
| 1.5×（2880×1620） | 342ms | 41ms |
| 2×（3840×2160） | 490ms | 52ms |

  单线程整块与逐像素耗时相近，收益几乎都来自并行；瓶颈在每像素的 f64 除法与取整，要逐字节一致就不能去掉。
- 新版 `--pin-presentation-smoke`、`--pin-smoke`（zoom，25／50／100／200%）通过。真实大贴图的滚轮手感未实测。

## 第二步：先备好底图再改窗口（10-16）

### 计时专项

`src/pin_zoom_timing_smoke.rs`（`pin_handoff_smoke` 的子模块）。运行：`rabshot-gpui --smoke`，环境 `RUSTSHOT_SMOKE_CASE=pin-handoff`、`RUSTSHOT_PIN_HANDOFF_QA=1`、`RUSTSHOT_PIN_ZOOM_TIMING_QA=1`、`RUSTSHOT_QA_NO_RESIDENT_NOTIFY=1`，`RUSTSHOT_DATA_DIR` 放在 `target/qa` 下且 `settings.ini` 写 `history=false`；可选 `RUSTSHOT_PIN_ZOOM_TIMING_SIZE=宽x高`（默认 1920x1080，超过当前屏 3/4 时缩小）。EXE 先复制出 `target/debug` 再运行；已有截图会话时会拒绝启动。

流程：合成冻结桌面 → 选区中心投递自有中键 → 真实 Action::Pin 首显 → 向贴图投递滚轮：放大 5 档、缩小 9 档、放回 4 档，最后连发 3 档测合并。后台线程约 0.25ms 轮询自有 HWND 的窗口矩形、GPUI Present 回执（`RustShot.GpuiPresentSerial／Size`）与内容完整标记（`pin_content_drawn_size`），不读像素、不动真实指针。结果写到 `target/qa/pin-handoff/*.jsonl`：`wheel_to_resize_ms`、`wheel_to_content_ms`、`resize_to_content_ms`（错位时长）、`mismatch_refreshes`（按显示器刷新率折算的帧数）及 `zoom_*` 阶段打点。

### 基线拆解

59Hz、125% DPI、1920×1080 选区、32 逻辑核：滚轮后约 2.5ms 窗口就改尺寸，约 34ms 后完整新内容才 Present，错位中位 32ms（p90 37、最大 49），合 2–3 帧。其中：

- `SetWindowPos` 约 1ms 内生效；
- GPUI 由 VSync 线程每帧投递 `RedrawWindow`，定位后要等下一次垂直同步才开始绘制，约 6–10ms；
- 绘制里按新倍率采样底图约 18ms（可见域被虚拟桌面裁掉一部分，比上表整块小）；
- 上传纹理与 Present 约 4ms。

### 改动

- `CornerCache` 增加一份预备底图（目标键 + 原图身份 + 未上传分块）。`stage_body` 按目标几何采样，不动已发布帧；`prepare_group_frame_masked` 每次都消耗预备：计划、`source_id`、可见域与原图身份全部一致才采用，否则退休。采样与现采共用 `sample_body_tiles`，像素逐字节相同，完整性标记与首显门不变。CPU 上最多当前帧加一份预备，仍在 384MB 上限内。
- `CaptureView::prestage_pin_presentation_body` 用目标窗口矩形走与实际帧同一套公式：物理宽高按 GPUI 的“÷DPI 再 ×DPI 取整”得视口，`pin_presentation_plan_for` 得计划，`pin_annotation::shared_preview_region_at` 得可见域。
- `PinView::request_placement` 的定位任务仍只处理最新一次请求。尺寸变了才先预备底图，再用 `pin_native::place_window` 只改主窗口；回调同步完后用 `RedrawWindow(RDW_INVALIDATE | RDW_UPDATENOW)` 立即画改尺寸后的首帧，不等下一次垂直同步，画完再 `pin_shadow::sync_native` 重绘阴影。纯移动仍走原 `place`（含阴影），不预备、不立即重绘，拖动移动保持垂直同步节奏。
- 阴影后移的原因：贴图是无边框窗口，用户看到的“窗口大小”就是阴影加内容；阴影是四条分层窗口，每次改尺寸要 CPU 重算并 `UpdateLayeredWindow`，约 4ms。原先它排在新内容之前，阴影先变、内容后到；后移后内容早约 4ms，阴影与内容的时间差也从约 9ms 缩到约 5ms。
- 没有放后台线程：采样仍在 UI 线程，耗时与原来在渲染里采样相同，只是挪到改窗口之前。UI 线程忙时排队的滚轮在下一次定位任务里只取最新请求，天然“最新优先”；后台方案在连续滚动时要么展示过期结果，要么反复丢弃重采而迟迟不出图，收益不抵复杂度。

### 结果

同机同专项（每组 19 步，最后一步为 3 档连发）：

| 指标 | 基线 | 现在（两次） |
|---|---|---|
| 滚轮→窗口改尺寸（中位） | 2.5ms | 19–20ms（先采样再改窗口） |
| 滚轮→完整新内容 Present（中位） | 34ms | 29–31ms |
| 窗口改尺寸→新内容，即错位（中位／p90／最大） | 32／37／49ms | 12–13／15–17／18–25ms |
| 按 59Hz 折算的错位帧数（中位／p90／最大） | 2／3／3 | 1／1–2／2 |

- 中间版本：只预备底图、仍等垂直同步时错位中位 15ms、p90 27ms；再加立即重绘为 12.7ms、p90 16.7ms；阴影后移后数值相近，主要收益是阴影与内容更接近同帧（专项不测阴影窗口）。
- 3 档连发只定位一次、只预备一次，直接到最终倍率。
- 剩余约 12ms：`SetWindowPos` 里 GPUI 处理 WM_SIZE（交换链 resize）约 2–3ms，定位后的回调同步与首帧绘制（含约 18MB 底图纹理上传）约 8–10ms。滚轮到出图的总延迟没有变长：UI 线程上的采样只是从绘制里挪到了改窗口之前。
- `cargo test --lib`：`pin_presentation` 53 项通过（新增 `staged_body_is_adopted_only_by_matching_frame_and_retired_otherwise`；`dual_4k_preview_preflight_*` 仍为既有失败）、`pin_zoom` 13 项、`pin_color_sample` 12 项通过；`--pin-presentation-smoke`、`RUSTSHOT_PIN_SMOKE_CASE=zoom --pin-smoke`（25／50／100／200%）通过。

### 方向二（GPU 采样）评估

预备之后错位已不含采样时间，剩下的是 `SetWindowPos` 内部与一次绘制，GPU 采样不会再缩小错位，只能把滚轮到出图的总延迟再降约一次采样（15–20ms）。代价：多屏冻结桌面是多张平面，GPU 双线性在平面接缝处要取相邻平面的像素，只能先合成整张纹理（最多 32M 像素）；光标叠层要先合成再采样；GPUI 图片采样是直通 alpha 线性，需在 vendor 着色器里加预乘采样；浮点插值与现有 CPU 舍入差 ±1，会打破 `unscaled_background_*` 像素合同、取色快照与导出一致性，完整性标记也要改为认可 GPU 底图。风险高、收益只在延迟，暂不做。

## 第三步：先出帧再移动窗口（10-19）

### 现象

用户反馈：按鼠标位置缩放时内容剧烈抖动。原因是改尺寸同时要移动窗口（锚点不在左上角），而旧流程先 `SetWindowPos` 再画：约一帧内 DWM 把旧倍率内容放在新原点上（内容整体平移 −dx、−dy），下一帧才换成新倍率。按左上角缩放原点不动，所以不明显。

### 改动

- vendor GPUI 新增 `Window::prepare_content_bounds`／`defer_next_present`／`present_deferred`／`wait_for_gpu`，见 [vendor 说明](../../vendor/README.md)。
- `PinView::request_placement` 改尺寸时（`pin_native::present_first_bounds`：无边框、目标矩形所在屏 DPI 与当前相同）：
  1. 预备底图（第二步），画布 `pin_render_placement` 记下目标矩形，可见域不读尚未移动的 HWND；
  2. `prepare_content_bounds` 让视口与交换链先变成新尺寸，bounds 观察者同步画布与工具栏；
  3. 推迟 Present 并立即重绘，`wait_for_gpu` 等新帧画完；
  4. `DwmFlush` 等一次合成结束，`present_deferred` 提交，再 `wait_for_gpu` 等 GPU 处理完这次 Present；
  5. `SetWindowPos`，紧接着重绘阴影。之后的 WM_SIZE 同尺寸，不再重建交换链。
  跨 DPI、有非客户区或 `SetWindowPos` 失败时退回原流程（失败时视口改回实际客户区）。纯移动不变。
- 第 3、4 步的原因：DWM 收取翻转 Present 比收取窗口位置慢（Present 后约 1ms 就移动时，常见“窗口已动、旧内容随之平移”）；在一次合成刚结束时提交，并等 Present 被 GPU 处理，两者离下一次合成都有余量。

### 合成帧检查

`RUSTSHOT_PIN_ZOOM_COMPOSITE_QA=1` 加在缩放计时专项上（`src/pin_zoom_composite_probe.rs`）：底图加一条竖条、一条横条标记；后台线程用 DXGI 桌面复制取贴图所在屏的每个合成帧，只复制穿过检查点的一行一列像素（不保存图像），测贴图外框与标记位置。外框不属于该档缩放前后任一矩形（超出工作区的边落在工作区与屏幕边界之间即可，任务栏可能在贴图上方或下方），或标记相对外框的比例与首帧差超过 3px，记为错位帧。贴图大到超出屏幕右下时这两种错位都测不到（第 2–7 档），所以只算可测的档。`RUSTSHOT_PIN_ZOOM_PLACE_FIRST=1` 恢复旧的先移动再绘制，用于同一 EXE 对照。

### 结果

同机（2560×1440、125% DPI，桌面复制帧间隔约 4ms），每组 19 档：

| 流程 | 错位合成帧（每组） | 滚轮→新内容（中位） | 滚轮→窗口变化（中位） |
|---|---|---|---|
| 旧：先移动再绘制 | 31（11 档，每档 2–4 帧） | 37ms | 20ms |
| 先画、等 GPU、立即移动 | 3 | 28ms | 33ms |
| 再加合成对齐 + Present 后等 GPU（采用，5 组） | 1、2、1、0、1 | 27–32ms | 34–41ms |
| 同上但去掉 `DwmFlush`（3 组，与采用版交替） | 2、2、3 | 26–28ms | 33–35ms |

- 剩下的错位多在第 0、1 档（窗口最大的两次放大），表现为窗口已移动、新帧未到一帧。推测是交换链刚重建后的首次 Present 被 DWM 收取更慢；要彻底消除需要窗口位置与内容原子提交，DComp 交换链没有这样的接口。
- 新内容出现得比旧流程早（不再等移动后的回调同步）；窗口变化晚约 15ms，但与内容同帧出现，手感上不再是“先变形后补画”。
- 计时专项原有指标 `resize_to_content_ms` 现在为负（内容先于窗口矩形），`mismatch_refreshes` 为 0。

## 局限与后续

- 合成帧检查仍有约 1／19 档出现一帧错位（见第三步），多在大窗口首几次放大；交换链若按档预留尺寸、不每档重建，可能进一步减少，改动面大，暂不做。
- 逻辑核少的机器采样更慢，滚轮到出图的总延迟随之变长，但窗口与内容仍基本同步出现。
- 带大量标注的贴图每档缩放都会整体重建标注缓存（投影变化），成本与旧的逐帧直接绘制相当。
- 真实鼠标的大贴图滚轮手感未实测；拖角改尺寸同样走预备路径，未单独计时。
