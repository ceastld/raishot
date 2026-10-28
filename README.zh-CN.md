<div align="center">
  <img src="assets/app.svg" width="88" height="88" alt="RaiShot 图标">
  <h1>RaiShot</h1>
  <p><strong>截图、长截图、贴图与录屏，专为 Windows 打造。</strong></p>
  <p>
    <a href="https://github.com/ceastld/raishot/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/ceastld/raishot/ci.yml?branch=main&amp;label=CI" alt="Windows CI"></a>
    <a href="https://github.com/ceastld/raishot/releases/latest"><img src="https://img.shields.io/github/v/release/ceastld/raishot?color=078267" alt="最新版本"></a>
    <a href="https://github.com/ceastld/raishot/releases/latest"><img src="https://img.shields.io/badge/Windows-10%20%2F%2011%20x64-0078D4" alt="Windows 10 和 11 x64"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-078267" alt="MIT 许可证"></a>
  </p>
  <p><strong><a href="https://github.com/ceastld/raishot/releases/latest">下载 Windows 版</a></strong> · <a href="README.md">English</a> · <a href="CHANGELOG.md">更新记录</a> · <a href="docs/README.md">文档</a></p>
</div>

RaiShot 让你截取屏幕上的内容、标出重点，直接复制或保存。需要对照资料时，把图片或文字贴在屏幕上；内容超出一屏时，拼成长图或录成视频。常用操作都能从托盘和快捷键进入。

使用 **Rust + GPUI** 开发，采用 Windows 原生截图与录屏接口。界面支持**简体中文和英文**，浅色、深色主题默认跟随系统。

## 功能

| 功能 | 可以做什么 |
| --- | --- |
| **截图** | 截取区域、窗口或多个屏幕，在复制或保存前调整选区。 |
| **标注** | 添加箭头、图形、文字、高亮、马赛克和模糊；选中后继续编辑，支持撤销与重做。 |
| **长截图** | 手动或自动滚动拼接，支持纵向、横向，以及反向滚动回看已截取的内容。 |
| **贴图** | 把图片或文字置顶显示，支持缩放、标注、透明度、鼠标穿透和恢复最近关闭的贴图。 |
| **OCR 与二维码** | 在本机识别文字、复制结果，识别或生成二维码。 |
| **录屏** | 录制带系统声音或麦克风的 MP4，或导出 GIF；支持鼠标点击和按键提示。 |

**图库**集中管理截图、贴图和录屏，提供图片与视频预览，以及图片对照、拼接、批量导出等工具。

### 超过一屏，也能截完整

网页、聊天记录、横向文档，都可以边滚动边查看拼接结果。你可以自己滚动，也可以交给自动滚动；反向回看已经截过的内容时，不会简单地重复追加。

拼接会在对齐画面、选择接缝时处理固定页眉和悬浮元素。算法设计与当前验证范围见[长截图专题](docs/references/long-capture-floating-elements-2026-10-26.md)。

## 快速上手

**系统要求：Windows 10 / 11，x64。**

1. 前往[最新发布页](https://github.com/ceastld/raishot/releases/latest)，下载 `RaiShot.*.zip`。
2. 将压缩包**完整解压**到一个文件夹，运行 **`raishot.exe`**。保留包内 EXE 和 DLL 的同目录关系。
3. 按 **Ctrl+1** 截图，按 **Ctrl+2** 将剪贴板中的图片或文字贴到屏幕上。两项快捷键都可以在设置中修改。

单击托盘图标开始截图；右键打开图库、设置和其它工具入口。Windows N 版本使用录屏需要安装媒体功能包。

### 先记住这几个快捷键

| 场景 | 快捷键 | 操作 |
| --- | --- | --- |
| 全局 | **Ctrl+1** | 开始截图 |
| 全局 | **Ctrl+2** | 剪贴板贴图 |
| 截图选区 | **L** / **G** | 长截图 / 录屏 |
| 截图选区 | **R** | 文字识别 |
| 截图选区 | **Enter** / **Ctrl+S** | 复制并完成 / 保存 |
| 截图中 | **Esc** | 关闭当前面板、取消操作或退出截图 |

以上全局快捷键为新配置的默认值；其余按键适用于普通截图选区，文字编辑、OCR 等活动工具优先处理。完整快捷键、各场景的按键行为和设置说明见[使用指南](docs/behavior.md)。

### 本机数据与 OCR

设置、贴图工作区、可选的截图历史、录屏和 OCR 缓存位于 `%LOCALAPPDATA%/RaiShot`。截图历史可在设置中开启。

文字识别在本机运行，首次使用会下载所需资源，并校验固定大小和 SHA-256。在线翻译是独立的可选功能，使用你配置的服务。

## 从源码构建与参与贡献

在 Windows 上准备 **Rust 1.90+（MSVC）**、**Visual Studio C++ Build Tools** 和 **Windows SDK**：

```powershell
git clone https://github.com/ceastld/raishot.git
cd raishot
cargo build --locked
```

GPUI 使用仓库中锁定的版本，本地补丁见[依赖说明](vendor/README.md)。构建与测试方式、架构、技术取舍和当前工作都在[开发文档](docs/README.md)中维护。

欢迎提交问题、复现样本、翻译和代码改进。参与前请阅读[贡献指南](CONTRIBUTING.md)，问题与功能建议提交到 [Issues](https://github.com/ceastld/raishot/issues)；涉及安全的报告请按[安全说明](SECURITY.md)处理。

## 文档

- [使用指南与快捷键](docs/behavior.md)
- [更新记录](CHANGELOG.md) · [当前状态与已知限制](docs/status.md) · [开发路线](docs/roadmap.md)
- [构建说明](docs/development.md) · [发布流程](docs/releasing.md) · [架构](docs/architecture.md) · [技术专题](docs/references/README.md)

首页介绍主要使用方式，各发布版本的具体变化以更新记录为准；当前源码中的进行中工作及验证范围见状态页。

## 许可证

RaiShot 采用 **[MIT 许可证](LICENSE)**。第三方依赖、模型、字体和其它资源保留各自许可证，详见[第三方声明](THIRD_PARTY_NOTICES.md)与[许可说明](docs/license.md)。
