# RaiShot Quicker 集成

动作只负责命令路由、下载与校验、启动独立 RaiShot。V1/V2 共用 `cea.rustshot` 原生 EXE 包；目标电脑不需要维护工具或额外的 .NET 桥接包。

动作源名称为“**raishot**”。默认点击直接框选，右键提供“区域截图”“启动托盘”“检查程序文件”。支持参数 `region`、`tray`、`verify`，显式参数优先；`verify` 只下载和校验，不启动程序。线上当前为 RaiShot **0.4.16**，见[本版更新](release-notes/0.4.16.md)与[动作分享链接](https://getquicker.net/Sharedaction?code=8cb3e0ee-148d-4879-51f3-08df2d739eae)。

程序包、OCR 与表格模型的来源、下载量、缓存和下载保护见[下载说明](DOWNLOADS.md)。

动作使用 `raishot.exe`／`raishot-gpui.exe`／`raishot-cli.exe`，导入文件为 `RaiShot.v1.json`／`RaiShot.qka`。升级前请正常退出旧托盘，再点击动作启动新版；V2 账号动作更新后由客户端同步拉取。开发验证可显式使用 `-DevDirectory`。

维护已有 V2 账号动作使用 `Update-AccountAction.ps1`：默认导出与预检，显式 `-Apply` 才写入账号同步服务器，之后由 Quicker 同步拉取；`-MetadataOnly` 仅同步外观和右键菜单，适合新包发布前改名。新草稿仍使用原生 MCP；V1 保留 `install-quicker-action.ps1 -Backend V1 -WorkspaceRoot <工作区>` 适配。

更新已有动作默认保留用户当前图标；显式传入 `-UpdateIcon` 才替换为动作源的图标。新建动作直接使用动作源图标。

Release 默认由 [GitHub Actions](../.github/workflows/release.yml) 构建：手动运行填写 `package_version` 后生成可下载的便携包产物；推送对应版本标签才创建 GitHub Release。CI 要求标签／输入版本与根 Cargo 版本一致，新建 Release 优先采用 `CHANGELOG.md` 对应版本的小节。日常开发使用 Dev 增量构建，构建与验证约定见[开发文档](../docs/development.md)。

Quicker 依赖发布从已有 Release 产物开始，使用 `Publish-Dependency.ps1 -PackageVersion <新版本>`；默认只准备和完整核验包，`-Upload` 上传私有依赖，`-Upload -Public` 上传公开依赖。同版本内容不同会停止，不覆盖远程文件。动作账号更新、公开分享与 GitHub Release 分开执行。

离线验证：`pwsh ./quicker/Test-Action.ps1`。项目开发入口见[文档索引](../docs/README.md)，功能差距和后续工作见[开发路线](../docs/roadmap.md)。

版本变化见[更新记录](../CHANGELOG.md)，动作页面内容见[动作说明源](SHARE.md)。发布后分别验证账号同步、程序下载、长时间闲置后的触发和跨账号完整安装。

## 动作流程

动作先查找隐藏托盘：已运行时，`region` 直接 PostMessage，`tray` 直接结束，不访问依赖目录、不查询进程路径、不创建启动进程。未运行或手动 `verify` 时，才使用 V1/V2 共有的 `sys:dependencycheck` 下载并复用 `cea.rustshot` 缓存；未运行时启动 `raishot.exe`，完整清单及 SHA256 校验仅在 `verify` 时执行。动作没有 C# 步骤。默认直接框选，首次启动也会截图；可用 `launch_mode` 或显式 `tray` 参数改为仅启动托盘。

RaiShot 使用同一个原生 EXE 包，动作只使用 V1/V2 共有内置步骤；目标电脑只需 Quicker 和包内文件，运行不需要 MCP、RPC 插件或额外 .NET 宿主。`raishot-gpui.exe --region` 保留为手动直接启动入口。动作复用现有托盘，与程序自身的单实例行为一致；升级程序需先正常退出旧托盘，再启动动作。

测试目标为 Windows 10/11 x64 和可用 D3D11；HDR 截图转换为 sRGB。VC++ 运行库在应用同目录部署；截图不需要在目标电脑安装 Rust、编译工具或独立 .NET 运行时。Quicker 动作本身运行于 Quicker。录屏使用系统 Media Foundation，Windows N 需安装媒体功能包；OCR 模型与字典按需下载。打包入口读取已构建的三个 EXE，不会自行修改或重新编译程序。

## 制作与本机安装

在项目根目录执行；新依赖发布并同步动作版本后，再运行安装入口：

```powershell
# 先准备新依赖包，上传步骤见顶部发布说明：
pwsh ./quicker/Publish-Dependency.ps1 -PackageVersion <新版本> -SourceDirectory <Release产物目录>
# 默认优先本机已启用的 V2 原生 MCP：
.\install-quicker-action.ps1
# V1 开发机的次要入口：
.\install-quicker-action.ps1 -Backend V1 -WorkspaceRoot <工作区>
```

`package-quicker.ps1` 默认包版本读取动作源；正式准备新包应显式指定尚未占用的三段版本，输出 `RaiShot.<版本>.0.zip`，不能使用保留的旧分发版本。ZIP 根目录包含三个 EXE、运行库、许可证、`manifest.json` 与说明，不套外层目录。脚本禁止覆盖同版本的不同内容。依赖可下载后再同步两份动作源的 `package_version`；程序运行版本与分发包版本分别记录在清单中。

`-Backend Auto` 为默认：本机原生 MCP 配置已启用时选择 V2，未配置/禁用时选择 V1；也可显式指定 `V2` 或 `V1`。选择 V2 后遇到错误不会在写入一半时切换另一个实例。V2 通过原生 MCP 打开/创建暂存动作、编辑 Source YAML 与文件后保存，更新已有文件先读取再作最小文本修改。MCP 凭据只在本机读取，不随包或动作分发。新动作保存在暂存区；未指定场景时由用户在 Quicker 选择“保留到…”。

V1 使用已安装的 qkrpc CLI；临时编辑副本在 Workspace 的 `.tmp/rustshot-action/`。传入 `-ActionId` 更新本脚本此前创建的动作时，必须选择对应实例的 `-Backend`。V1 先导出并绑定 `editVersion`；V2 使用原生暂存区版本/冲突流程。

V1 入口同时导出 `dist/quicker/RaiShot.v1.json`（完整兼容动作）和 `RaiShot.qka`（V1 设计器正文）。另一台电脑在 Quicker 空白位置右键选择“导入动作”，选择 `.v1.json`；V1 直接使用，V2 官方入口将其转成结构化动作。文件中的校验已内嵌，不需要复制本机开发脚本。V2 原生入口另外保存 `dist/quicker/RaiShot.source/` 源副本和 `RaiShot.v2-local.json` 本机收据；收据不是动作导入文件，也不是 V2 原生 JSON 导出。

V2 主维护源为 `quicker/action/program.source.yaml`、`info.yaml`；V1 适配源为 `data.json`。两者共用手动检查使用的 `files/validate-package.value.txt`。改包名/版本、变量或步骤时必须同步两份声明，更新兼容导入文件并核对共有流程；不要把临时单版改动误称为两版兼容。`.value.txt` 是含 `$=` 的完整参数值，不能改回 `.eval.cs` 后缀（后者的原生资源 lint 只接受无前缀 C#）。

### 本机 Dev 动作

日常调试先将 Debug 三个 EXE 及所需运行库复制到独立运行目录，再更新本机 V1 动作：

```powershell
pwsh ./install-quicker-action.ps1 -Backend V1 -WorkspaceRoot <工作区> -ActionId <本机动作GUID> -DevDirectory <开发运行副本目录>
```

开发动作跳过远程依赖下载，热托盘通知与冷启动流程保持一致；`verify` 只检查本地三个 EXE 是否存在，不要求发布清单，也不启动程序。可加 `-VerifyOnly` 在安装后执行该检查。导出默认放在 `target/quicker-action/dev/`，自定义输出也须位于本项目 `target/`；这些文件含本机路径，仅供本机使用。该参数必须显式指定 `-Backend V1`，不应用到 V2 或账号动作；不能直接指向 `target/debug/` 或 `target/release/`，以免锁住编译产物。正式 Release 与依赖包后续走 CI。

## 分发依赖

使用 `quicker/Publish-Dependency.ps1` 从已有 Release 产物准备新包。脚本默认只打包和完整核验，显式指定上传参数才向依赖服务提交：

```powershell
# 准备并核验新包：
pwsh ./quicker/Publish-Dependency.ps1 -PackageVersion <新版本> -SourceDirectory <Release产物目录>
# 上传公开依赖：
pwsh ./quicker/Publish-Dependency.ps1 -PackageVersion <新版本> -SourceDirectory <Release产物目录> -Upload -Public
```

省略 `-Public` 时上传私有依赖；依赖文件的下载地址仍不应作为保密存储使用。动作账号更新、公开分享和 GitHub Release 分别执行、分别验证，不把打包成功当作全部发布成功。

默认输出为 `dist/quicker/RaiShot.<版本>.0.zip`。此目录由打包和安装脚本生成，公开源码仓库不预置分发产物。依赖可下载并核验后，再同步 V1/V2 动作源中的 `package_version`，保持包名 `cea.rustshot` 不变。

Quicker 缓存位于系统“文档”目录的 `Quicker/_packages/cea.rustshot/<三段版本>/`。依赖步骤使用最低版本，并优先选择已有的较新版本；动作检查清单的包名、架构、版本与每个必需文件的大小及 SHA256，避免缺件或文件混用。这是完整性检查，不是发布者数字签名。不要覆盖同版远程包来期待所有电脑刷新缓存。

维护用 `verify` 参数只下载和检查依赖，不启动截图。V1/V2 的 `-VerifyOnly` 应将输出指向项目内的 `target/qa/`，检查实际执行日志，并确认正常动作源保持原有启动条件。已有缓存验证、联网首次下载、异机运行和截图界面是不同验证范围，应分别记录。
