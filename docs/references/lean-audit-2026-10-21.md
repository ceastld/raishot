# 精简审计（2026-10-21）

## 方法

按“正确 → 安全 → 负载 → 测试 → 速度 → 精简”的顺序审，每条结论都要能说出具体会出错的输入或场景；说“没用”之前全仓库搜过（含测试、smoke、实验模块）。修复只取能删代码的最小改法，不加新层。

`src/` 约 37.7 万行，不逐文件通读：
- 数据写盘最重的 `pin_workspace.rs`（贴图清单、租约、资源事务）整段精读；
- 其余用重复块扫描（非测试代码中规范化后连续 ≥12 行相同）加编译器的 dead_code 警告定位，再逐处核对。

## 已改

| 范围 | 改法 | 行数 |
| --- | --- | --- |
| `pin_workspace.rs` | 截图快照与截图贴图两条保存路径共用 `PendingAsset::encode_in`（超限提示统一为本地化文案）；资源“必须是普通文件”的 4 处校验合成 `ensure_regular_asset`，`claim` 里对同一文件的重复校验去掉；`save_snapshot_with_text_document` 里与正常分支等价的提前返回去掉，随之无人调用的 `save_snapshot_with_encoder` 删除 | −48 |
| `gdi_surface.rs`（新） | 贴图标题条与录屏小浮层原各有一份离屏 GDI 位图（DC、字体、DIB、圆角描边预乘、Drop），合成一个 `Surface`；两边只保留各自的版面与绘制 | 两文件 −341，新文件 +231 |
| 同上 | 12 处手写的“32 位自上而下 DIB 位图头”改用 `gdi_surface::bitmap_info`（原 `startup_surface.rs` 私有版本上移） | −125 |
| `fast_capture.rs` | DIB 与 DDB 两种设备捕获共用开头（DPI 守卫、设备 DC、映射几何、首段计时）抽成 `open_device` | −20 |
| `startup_bitmap.rs` | DIB／DDB 两个构造共用 `owning`（先取得所有权再校验，失败由 Drop 回收） | −8 |
| `startup_output.rs` | 3 处延迟预览回执校验、2 处立即预览确认校验各合成一个辅助函数 | −8 |
| `capture_canvas.rs` | 绘制循环里 5 处“物理像素矩形 → 窗口逻辑坐标”换算合成 `scaled` 闭包 | −45 |
| `pin_properties.rs`／`pin_tool_picker.rs` | 两个工具栏弹层共用 `popup_placement`（下方优先、放不下翻上方）与 `own_popup`（工具窗样式与归属） | −22 |
| 死代码 | 删除无生产调用方的 `ocr_panel_visible`、`CaptureStandby::request`、`image_editor::open_crop`、`ocr_panel::table_rows`、`table_export::markdown`、`property_items::DIGIT`、`FinishEffect::enable/toggle`、`DownloadProgress::percent`（及只测它们自己的测试）；录屏码率实验用的 `Mp4Writer::new_tuned_for_lab` 改为仅测试编译 | −190 |

合计约 −560 行，行为不变（唯一可见差异：截图贴图资源超限时的提示由中文内部文案换成与普通贴图一致的中英文提示）。`cargo check --lib --tests` 不再有本次范围内的警告，原有 9 条 dead_code 警告清零。

## 发现但未改

1. **OCR 局部行候选两份近似实现**（已合并宿主无关部分，真实界面未验）：`capture_ocr_local_retry.rs`（3350 行）与 `pin_ocr_local_retry.rs`（2598 行）原有十几段 14–36 行相同或近似的块。
   - 共用实现放在 `ocr_local_candidate.rs`（挂在 `capture_view::local_retry::candidate`）：泛型候选面板 `CandidatePanel<S>`（`painted_frame`／`visual_current`／`move_vertical`／`styled_text`／`button_stamp`／`paint_stamp`）、按钮快照 `CandidateButtonStamp<S>::matches`、排队命令、拖选状态、真实 paint 回执 `CandidatePaint::publish`（发布到 `VisualCache` 的 publish／publish_without_rows／invalidate 三分支）、`CandidateScrollPaint<R>::current`、视口夹取、拖选延伸 `drag_tick`、边缘滚动与 reveal 几何（原 `pin_candidate_scroll.rs` 与截图版 `candidate_edge_offset`／`candidate_reveal_offset` 合一）、复制回执文案、固定输入 `retained_image`／`retained_result`。主键按下判断复用 `find_ui::primary_button_down`。
   - 差异仍由宿主提供：贴图回执用 `CandidateScrollPaint<Option<Arc<CandidateRows>>>` 携带 `candidate_painted_rows` 行停点（截图版 `R = ()`，不计算）；就绪门、焦点、鼠标事件阶段（截图 bubble／贴图 capture）、`rehit`／`affinity`、重试点击、面板位置与提示文案、`target_error` 措辞都留在各自宿主，未强行统一。
   - 顺带：查找栏的新建与上一处／下一处并入 `FindUi::open`／`FindUi::cycle`；敏感候选列表项文案与已选计数并入 `Preview::item_label`／`selected_count`。
   - 行数：局部行候选两宿主 −733、删 `pin_candidate_scroll.rs` −105、共用模块 +562（含从截图版迁来的两组滚动几何测试），净约 −273；查找 +4（只换单一来源），敏感 −7；合计约 −276。
   - 唯一数值差异：截图版 reveal 的“行高大于视口”判断由 f32 相减改为与贴图版一致的 f64 相减，只在亚像素舍入边界上可能不同。
   - 已验：`cargo check --lib --tests` 无本范围警告；`cargo test --lib` 过滤 local_retry／ocr_find／ocr_sensitive（82 通过，1 个既有 ignored）及 find_click／find_ui／sensitive（53 通过）。
   - 真实界面未验：截图与贴图的局部行候选（拖选、边缘自动滚动、上下键、Ctrl+C／按钮复制与重试、滚轮中断、失焦取消）、查找栏打开与 F3 循环、敏感候选列表显示；只做了编译与单元测试。
   - 仍平行未并：`run_local_ocr` 的后台识别与回执处理（状态结构不同，各 ~40 行）、两版拖选按下／抬起与 tick 的就绪条件。`capture_view.rs` 与 `startup_surface.rs` 里还各有一份主键按下判断，未在本次范围内替换。
2. **贴图文字／图片两套历史 DTO**（可以修）：`pin_text_history.rs` 与 `pin_raster_history.rs` 的命令枚举与转换有 14–20 行的平行块，差别只在对象类型。涉及磁盘格式，收益中等，暂不动。
3. **显示器吸附测试与代码不一致**（已修）：原生首屏路径的 `startup_surface_input.rs::monitor_snap_down` 要求“Ctrl+A 且不按 Shift”，编辑界面的 `snap_screen_preview` 却接受 Ctrl+Shift+A，3 个吸附测试按后者写而失败。维护者确认 Ctrl+Shift+A 也应吸附：去掉 Shift 限制，判定表测试里那一行的期望同步改为接受。
4. `capture_properties.rs` 文字样式按钮表（斜体／下划线／删除线）在两处各写一遍，属数据表重复，合并收益小。

## 验证与未验

- `cargo test --lib` 过滤 pin_workspace、gdi_surface、pin_title、recording_tip、fast_capture、startup_*、raster、cursor、input_overlay、pin_shadow、pin_live、selection_finish、download_progress、table_export、ocr_panel、startup_output、capture_canvas、pin_properties、pin_tool_picker：全部通过（含第 3 条修复后的吸附测试，startup_surface／startup_output／monitor_snap 共 156 项）。
- 真实界面未验：贴图标题条、录屏说明气泡／消息条的像素、截图画布绘制、两个贴图弹层位置；托盘到可框选的启动时延未复测（启动路径只做了等价抽取）。
