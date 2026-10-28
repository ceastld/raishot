# 开发专题索引

这里集中保存项目的设计取舍、研究结论和相关验证，供贡献者和 agent 共用。先按功能找到专题，再搜索任务关键词，只读相关段落；不按日期通读，也不把旧版本的验证结论当作当前版本已通过。

当前任务与下一步只写在[当前状态](../status.md)，功能差距与迭代顺序只写在[路线](../roadmap.md)，当前行为与快捷键以[行为清单](../behavior.md)为准。专题保留设计、取舍和相关验证，不重复维护任务状态；旧计划与审计按当时范围理解。

本目录与[研究记录](../research/README.md)、[设计原型](../design/README.md)一起构成公开开发上下文。理解和继续开发项目不需要维护者的本机资料。许可说明见[许可](../license.md)。

## 截图与标注

- [基础体验差距](../research/capture-basics-gap-2026-10-12.md)
- [交互细节](capture-interaction-polish-plan-2026-10-14.md)
- [范围快捷键与选区保护](capture-range-shortcuts-2026-10-11.md)
- [截图后快捷动作](capture-quick-actions-2026-10-12.md)
- [外观面板与中键贴图](capture-finish-middle-pin-2026-10-12.md)
- [会话异常与恢复](capture-session-failure-2026-10-21.md)
- [初始设备恢复](capture-device-recovery-2026-10-12.md)
- [HDR 截图](hdr-capture-2026-10-19.md)
- [标注预览可见性](annotation-visibility-2026-10-12.md)
- [大量标注渲染缓存](annotation-render-cache-2026-10-15.md)
- [选区工具栏延迟](toolbar-selection-latency-2026-10-12.md)
- [矩形独立圆角](rectangle-corner-radii-2026-10-12.md)
- [效果圆角](effect-radius-2026-10-11.md)
- [嵌套组合与层内编辑](nested-groups-2026-10-11.md)
- [智能马赛克候选](smart-mosaic-persistent-plan-2026-10-09.md)
- [长截图动态接缝](long-capture-dynamic-seam-2026-10-12.md)
- [长截图悬浮元素](long-capture-floating-elements-2026-10-26.md)
- [长截图完成流程](long-editor-redesign-2026-10-20.md)
- [接缝对齐性能](seam-alignment-performance-2026-10-24.md)

## 贴图与编辑

- [主界面与数据模型](main-window-library-2026-10-19.md)
- [图库工具入口](library-tools-2026-10-23.md)
- [图片工具工作流](image-tools-workflow.md)
- [剪贴板贴图落点](clipboard-pin-placement-2026-10-21.md)
- [透明贴图与鼠标缩放](pin-clipboard-alpha-2026-10-16.md)
- [内容原点与恢复](pin-content-origin-2026-10-11.md)
- [级联右键菜单](pin-context-menu-2026-10-12.md)
- [中键贴图首显](pin-first-show-latency-2026-10-12.md)
- [混合文档保存恢复](pin-mixed-text-history-2026-10-11.md)
- [图像处理历史](pin-processing-history-2026-10-11.md)
- [富文本贴图](pin-rich-text-2026-10-19.md)
- [可视标题](pin-title-2026-10-19.md)
- [完整矢量文档](pin-vector-document-next-2026-10-12.md)
- [贴图缩放性能](pin-zoom-latency-2026-10-16.md)
- [文字贴图渲染性能](text-pin-render-performance-2026-10-19.md)
- [实时窗口贴图](window-pin-live-2026-10-12.md)
- [完整字素编辑](text-grapheme-edit-2026-10-12.md)
- [文本框布局与裁剪](text-field-layout-2026-10-24.md)

## OCR 与翻译

- [原位 OCR 与迷你栏](ocr-inline-release-2026-10-12.md)
- [普通图片原位 OCR](image-editor-inline-ocr-2026-10-12.md)
- [文字框与段落](ocr-boxes-and-paragraphs-2026-10-24.md)
- [文字框高审查](../research/ocr-ink-audit-2026-10-24.md)
- [段落合并审查](../research/ocr-paragraph-audit-2026-10-24.md)
- [中文续行复制](ocr-cjk-copy-seam-2026-10-11.md)
- [选字迷你标记栏](ocr-compact-mark-tools-2026-10-11.md)
- [标记栏状态反馈](ocr-icon-toolbar-polish-2026-10-12.md)
- [浮栏遮挡](ocr-toolbar-avoidance-2026-10-12.md)
- [标记与像素管线](ocr-mark-pipeline-2026-10-12.md)
- [译文来源与标记定位](ocr-source-and-marking-2026-10-11.md)
- [框选多选与结果筛选](ocr-result-filter-2026-10-23.md)
- [排除文字后填充](ocr-exclude-fill-2026-10-23.md)
- [OCR 性能对比](ocr-performance-2026-10-13.md)
- [进程内 OCR 研究](../research/ppocr-inprocess-research-2026-10-09.md)
- [多二维码识别](qr-multi-detect-2026-10-12.md)
- [表格识别](../research/table-recognition-2026-10-12.md)
- [翻译设置](translation-settings-2026-10-11.md)
- [图片翻译](image-translate-2026-10-12.md)
- [API 公式识别与翻译](deepseek-formula-translation-2026-10-24.md)
- [API 识别与翻译实测](../research/deepseek-api-results-2026-10-24.md)
- [公式、表格与翻译调研](../research/formula-table-translation-research-2026-10-24.md)
- [公式与离线翻译对比](../research/model-ab-results-2026-10-24.md)

## 录屏与媒体

- [媒体查看与工具路线](media-viewers-recording-tools-plan-2026-10-22.md)
- [媒体并行开发协调](../research/media-parallel-development-2026-10-22.md)
- [媒体与工具调研](../research/snow-apps-media-tools-research-2026-10-22.md)
- [动画图片与解码预算](image-animation-streams.md)
- [图片格式评估](image-format-evaluation.md)
- [音频输入与倍速](media-audio-processing.md)
- [录屏切换与预览](recording-handoff-preview-2026-10-13.md)
- [录屏输入显示](recording-input-display-2026-10-23.md)
- [录制中绘图](recording-live-drawing-2026-10-12.md)
- [录屏输出生命周期](recording-output-lifecycle-2026-10-11.md)
- [录屏引擎回滚](recording-producer-rollback-2026-10-12.md)
- [录屏工具栏与提示](recording-toolbar-tips-2026-10-20.md)

## 基础平台与性能

- [文件与贴图参数](file-launch-2026-10-12.md)
- [全局键鼠触发器](global-triggers-2026-10-22.md)
- [快捷键系统](shortcut-system-2026-10-27.md)
- [宿主修饰键隔离](host-shortcut-modifiers-2026-10-12.md)
- [设置文件与迁移](settings-json-2026-10-18.md)
- [托盘弹层](tray-popup-2026-10-24.md)
- [UIA 路径与取消](uia-path-order-2026-10-12.md)
- [远程画面兼容](remote-display-2026-10-22.md)
- [CI 构建性能](release-build-performance-2026-10-19.md)

## 研究、检查与发布

- [品牌与图标](brand-naming-and-icon-2026-10-16.md)
- [功能对照来源](../research/feature-gap-2026-10-19.md)
- [细节自查](detail-audit-goal-2026-10-11.md)
- [上线前交互检查](launch-interaction-audit-2026-10-21.md)
- [精简审计](lean-audit-2026-10-21.md)
- [人工测试交接](manual-test-handoff-2026-10-22.md)
- [长截图算法对照](../research/long-capture-algorithm-review-2026-10-24.md)
- [长截图宣传演示](promo-long-capture-2026-10-24.md)
- [旧功能实施计划](../research/next-features-plan-2026-10-12.md)
- [编辑与采集交付](editing-capture-release-2026-10-12.md)
- 开源整理与发布边界（历史原始记录，非开发依赖）
- Quicker 动作发布（历史原始记录，非开发依赖）
