# 图片翻译阶段（2026-10-12）

## 设计

- UI 原型：[design/image-translate.html](../design/image-translate.html)（深色默认、跟随系统主题），发布页 https://claude.ai/artifact/HeuMZPN5dm5kYRovqT9rfh 。
- 原则：本机 OCR，只把段落文字发往设置 ▸ 翻译的 OpenAI 兼容接口；图片、剪贴板不离开本机。译文是文档里的矢量对象，预览与复制／保存／贴图共用同一份数据；取样颜色来自截图像素，不随界面主题变化。
- 不采用整图翻译接口（上传图片、返回位图再替换）的做法；本项目不上传图片、不生成译图位图。

## 第一阶段（已实现源码）

| 部分 | 位置 | 内容 |
| --- | --- | --- |
| 核心 | `src/image_translate.rs` | 段落 → 翻译块（行四角合并、行高中位数、左／居中推断、倾斜检测）；跳过规则（数字符号、网址／路径／代码、倾斜竖排、已是明确目标语言）；环带中位底色与最强墨色取样；按原区域生成带不透明底色的文字标注并逐级缩字，最小 9px 时向下延伸 |
| 接入 | `src/capture_image_translate.rs` | 入口统一 `translate_image`；无原位结果时先走现有 OCR，回执安装后续接；3 线程逐块请求，进度写状态栏；全部返回后退出原位文字层，`insert_many` 一次插入（一次撤销）；完成前截图范围改变则放弃；会话内（目标，原文）缓存 |
| 入口 | `capture_toolbar.rs`、`capture_view.rs`、`capture_ocr_overlay.rs`、`capture_ocr_panel.rs` | “更多操作 ▸ 图片翻译 · Y”、截图 Y 键、原位文字层 Y 键、OCR 面板“更多 ▸ 翻译图片” |

取消：Esc 退出原位文字层（`dismiss_recognition_result`）即取消任务，剩余块不再派发，已发出的请求返回后丢弃。

## 未完成（后续阶段）

- 原型中的翻译面板（目标语言下拉、逐段卡片、单段重试／还原、译文／原图／对比视图、块操作条）。当前以状态栏进度和普通文字标注编辑代替。
- 主工具栏按钮（需同步工具栏宽度与 OCR 面板避让计算）。
- 译文组合为一组：当前每段是独立文字标注，同批一次撤销。
- 贴图入口（图片编辑与截图共用原位 OCR 宿主，已随截图入口可用，真实窗口未验）；复杂背景（渐变、照片）的填补；单行按钮类文字的居中；粗体推断；竖排文字。
- 变长语言（中译英等）在最小字号仍放不下时的延伸方向。

## 验证范围

- 2026-10-12 实际：独立目录 `CARGO_TARGET_DIR=target/image-translate` 下 `cargo test --lib image_translate` 6 项全部通过（分块、对齐与倾斜、跳过规则、负原点取色、贴边回退、缩字与延伸），随后 `cargo check --lib` 无错误无警告，两新文件 rustfmt 通过。首轮因并行会话 `gpui_capture.rs` 暂缺 `join_preparation` 失败，对方补齐后重跑通过；与本功能无关。
- 真实网络翻译、真实截图界面、IME、多屏和混合 DPI 均未验；不展示测试图片、不占前台。

## 百度图片翻译 V2 接入（2026-10-12）

- 用户要求：保留自研本机排版，同时接入百度「图片翻译 V2.0」整图接口（效果好、用户已申请）。
- 设置 ▸ 翻译 新增“图片翻译引擎”（自动／百度 V2／本机排版）与 APP ID、API Key；配置键 `imageTranslateEngine`、`baiduAppId`、`baiduApiKey`，只存本机。自动 = 两项凭证都填了就走百度，否则本机。
- `src/baidu_image_translate.rs`：选区内容（含已有标注）编码 JPEG（长边 ≤4096、≤3MB）→ `POST https://fanyi-api.baidu.com/ait/api/picture/translate`，`Authorization: Bearer <API Key>`，体含 `appid/from=auto/to/paste=1/view_type=1/model_type=nmt`；取 `data.paste_img` 解码并缩放回选区尺寸。错误码按百度接口文档映射。目标语言沿用文字翻译的“目标语言”，auto 按界面语言取 zh／en。
- 接入：`translate_image` 入口按引擎分流；百度结果在图片编辑窗口打开（可继续标注／复制／保存／贴图），截图会话随即关闭；贴图与图片编辑宿主保留原窗口。Esc 取消；范围改变则丢弃结果。
- 未完成：贴合图作为文档内图层（需要图片标注类型）而非新窗口；API Key 输入框未做掩码；真实接口未验。
