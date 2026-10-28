# 架构

RaiShot 将编辑模型、共享渲染和 Windows 界面分开。本文记录各层必须保持的约束；概念定义见[术语](glossary.md)。

## 按任务定位源码

| 任务 | 入口 |
| --- | --- |
| 托盘、启动与待命 | [tray.rs](../src/tray.rs)、[capture_standby.rs](../src/capture_standby.rs)、[gpui_capture.rs](../src/gpui_capture.rs) |
| 截图交互 | [capture_view.rs](../src/capture_view.rs)，具体设计见[截图专题](references/README.md#截图与标注) |
| 标注与历史 | 下方的 Annotation / Document / Editor / Scene 四个边界 |
| 贴图与工作区 | [pin_view.rs](../src/pin_view.rs)、[pin_workspace.rs](../src/pin_workspace.rs) |
| OCR | [ppocr/mod.rs](../src/ppocr/mod.rs)，管线、几何与结果过滤见[OCR 专题](references/README.md) |
| 翻译 | [translate.rs](../src/translate.rs)、[image_translate.rs](../src/image_translate.rs) |
| 录屏 | [recording.rs](../src/recording.rs)，输出、音频与媒体能力见[相关专题](references/README.md) |

当前未完成项先查[状态页](status.md)，功能差距与依赖先查[开发路线](roadmap.md)，避免根据旧研究重复实现已完成工作。

## 编辑模型

| 边界 | 职责 | 源码入口 |
| --- | --- | --- |
| Annotation | 标注的几何、样式与内容 | [annotation.rs](../src/annotation.rs) |
| Document | 对象集合与增量撤销命令 | [document.rs](../src/document.rs) |
| Editor | 手势状态、编辑预览与命令提交 | [editor.rs](../src/editor.rs) |
| Scene | 预览和导出共用的路径、色块与像素来源 | [scene.rs](../src/scene.rs) |

模型层独立于 GPUI 和 GPU 初始化。GPUI 负责输入接入和界面绘制，原生 WindowProc 负责转发事件。

一次手势结束只提交一个增量命令；取消和无变化操作不增加历史。删除、清空和批量编辑都应支持撤销。拖动中的预览留在 Editor，提交后的对象变化由 Document 管理。

## 坐标与选区

截图画布覆盖整个虚拟桌面，标注使用全屏物理像素坐标，包含负坐标、多显示器与高 DPI 情况。界面逻辑坐标在输入和绘制边界转换。

选区用于遮罩和导出裁剪。移动或重新选择范围不得平移、裁剪或清除文档中的标注。多屏工具栏锚定选区结束时的显示器；工具栏、辅助框和尺寸标签不进入导出。

## 渲染与贴图

预览和导出共用 Scene 的几何与效果结果。马赛克采样冻结像素及下层标注，色块按全屏原点对齐，导出裁剪保持相同颗粒位置。

矢量贴图以文档为准，复制、保存或拖出时再渲染成图片。文字贴图按需生成纯色背景和文字渲染结果，避免预生成整张背景位图。

主题调整界面外观，不改变截图、标注或导出像素。文字矢量贴图的纯色底和默认色文字可以跟随主题。

## 采集与进程

常驻进程管理托盘和快捷键，并预装载一个一次性待命进程。待命阶段不初始化 GPUI、应用 GPU 设备或 OCR，也不冻结桌面；收到请求后进入截图会话。截图完成或取消时，会话工作进程退出。

静态截图冻结只在初始化时允许整屏读回一次，禁止逐帧整屏读回；导出只读取所需选区。录屏、长截图等连续采集按需创建和释放采集资源。

每次成功的 DXGI Acquire 都必须对应 Release，包括后续处理失败和取消的路径。托盘退出只回收本实例创建的窗口与进程。

启动性能口径是托盘通知到真实冻结桌面可框选，目标约 200ms。只在修改启动路径时测量；`benchmark.ps1` 的 CLI 捕获耗时不能代替这一指标。

## 品牌与界面

产品名称、EXE 名、数据目录与开机启动项只在 `src/brand.rs`、`Cargo.toml` 的 `[[bin]]`、`build.rs` 和 `Tools/package.ps1` 定义，界面标题使用 `brand_title!` 或 `brand::NAME`。内部代号 `rustshot` 用于 crate、模块、源码文件名、窗口类、互斥量和 `RUSTSHOT_*` 环境变量，不随品牌变化；曾用名追加到 `brand::LEGACY_NAMES`，用于迁移旧数据。

界面文案使用 `tr!("中文", "English")`、`trf!` 或常量表 `i18n::Text`。语言 `system|zh-CN|en` 与主题 `light|dark|system` 默认跟随系统。固定宽度控件要检查英文溢出。

## 运行依赖与分发

Windows 界面使用锁定的 `vendor/gpui`，不依赖 .NET 或 WebView 运行时。修改依赖补丁前阅读[依赖说明](../vendor/README.md)。

OCR 的 ORT、模型和字典从固定官方来源后台下载，并按固定字节数与 SHA 校验，清单在 `dependencies/*.json`。运行缓存位于用户本机的 `%LOCALAPPDATA%/RaiShot/ocr`；发布包含 VC++ 运行库与 ORT，模型按需下载。

便携 ZIP 由 `Tools/package.ps1` 使用已有 Release 产物打包，脚本不构建、不上传。凭据只放仓库外配置或 CI Secret，不进入仓库、客户端或包。
