RaiShot 便携包

适用于 Windows 10/11 x64；解压后保留同目录的全部文件。
三个程序分别为 raishot.exe（托盘与启动入口）、raishot-gpui.exe（界面工作进程）、
raishot-cli.exe（命令行入口）。运行 raishot.exe --tray 启动托盘，
运行 raishot.exe --region 开始框选；命令行用法见 raishot-cli.exe --help。
升级前请从旧版托盘正常退出，再启动新版。

VC++ 与 ONNX Runtime 的 DLL 必须与 EXE 放在同一目录。
OCR 模型、字典与相关许可首次使用时按需下载，并按固定大小与 SHA256 校验。
设置、贴图、历史、录屏与 OCR 缓存位于本机 %LOCALAPPDATA%\RaiShot。
录屏使用系统 Media Foundation；Windows N 需安装媒体功能包，无需外部 FFmpeg。

manifest.json 记录包版本、架构与文件校验信息。
RaiShot 自有代码采用 MIT 开源许可证，允许商用、修改和再分发；
分发副本或实质性部分时须保留版权和许可声明。
许可全文见 LICENSE；第三方通知见 THIRD_PARTY_NOTICES.md，
ONNX Runtime 的许可与第三方通知也随包提供。
