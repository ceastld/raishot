RaiShot 便携包
分发包版本与程序运行版本分别记录于 manifest.json。

适用于 Windows 10/11 x64 与可用的 D3D11 显示设备；HDR 截图转换为 sRGB。
保留同目录的全部文件：三个 EXE、VC++ 运行库（vcruntime140、vcruntime140_1、msvcp140、msvcp140_1）
与 ONNX Runtime（onnxruntime.dll 及其许可文件），目标电脑无需另装运行库。
截图程序是独立 Rust + GPUI EXE，不依赖 Quicker、MCP、RPC 插件、.NET 或 WebView。
Quicker V1/V2 共用 cea.rustshot 包及 dependencycheck、assign、simpleIf、run 内置步骤。

运行动作默认直接框选；右键可选区域截图、启动托盘、检查程序文件。
region / tray 参数优先于动作默认方式；verify 参数只校验依赖，不启动截图。
命令行入口：raishot.exe --region / raishot.exe --tray。
使用新包前，请从旧版本托盘正常退出。已有常驻进程时，新入口会复用旧版本。

本包由当前工程的 Release 构建产物生成，包含截图标注、选区样式、贴图、
文字与表格识别、翻译、录屏及设置等功能。OCR 使用 Rust 进程内 PP-OCR。
ONNX Runtime 与 VC++ 运行库随包附带，模型与字典由程序按需下载、校验和缓存。
本包不附带新的启动耗时承诺；真实 GUI、混合 DPI/多屏与异机体验按实际使用验证。

manifest.json 记录架构、版本和各文件大小/SHA256；右键“检查程序文件”执行完整校验，普通启动复用依赖缓存。
包的上传、Quicker 两版验证和异机下载状态，以项目 quicker/README.md 与交付回执为准。
录屏使用系统 Media Foundation；Windows N 需安装媒体功能包，无需外部 FFmpeg。
ONNX Runtime 随包附带；PP-OCR 首次使用自动选择可用下载源并校验固定模型、字典及许可；只检测时先下载检测模型。
全文 OCR 再补识别模型/词典；Windows OCR 保留为显式提供者，需系统识别语言。
包内文件缺失时才回退为下载 ONNX Runtime；VC 运行库仍缺失会提示官方 x64 安装入口，不自动安装。
设置与贴图数据位于使用电脑自身的 %LOCALAPPDATA%\RaiShot，不随本 ZIP 分发。
包内不含开发机真实截图、剪贴板内容、账号、MCP 凭据或私人配置。
