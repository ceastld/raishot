# RaiShot Quicker 集成

动作只负责命令路由、下载与校验、启动独立 RaiShot。V1/V2 共用 `cea.rustshot` 原生 EXE 包；目标电脑不需要维护工具或额外的 .NET 桥接包。

动作源名称为“**raishot**”。默认点击直接框选，右键提供“区域截图”“启动托盘”“检查程序文件”。支持参数 `region`、`tray`、`verify`，显式参数优先；`verify` 只下载和校验，不启动程序。线上当前为 RaiShot **0.4.15**，见[本版更新](release-notes/0.4.15.md)与[动作分享链接](https://getquicker.net/Sharedaction?code=8cb3e0ee-148d-4879-51f3-08df2d739eae)。

程序包、OCR 与表格模型的实际来源、下载量、缓存和公开前测速见[下载说明](DOWNLOADS.md)。

当前动作使用 `raishot.exe`／`raishot-gpui.exe`／`raishot-cli.exe`，导入文件为 `RaiShot.v1.json`／`RaiShot.qka`。程序、两份 `package_version`、本机 V1、V2 账号及原分享均已更新为 **0.4.15**，保留原身份与图标。依赖已发布并通过独立匿名完整下载校验；V2 客户端需同步，升级前请正常退出旧托盘，再点击动作启动新版。开发验证可显式使用 `-DevDirectory`。

维护已有 V2 账号动作使用 `Update-AccountAction.ps1`：默认导出与预检，显式 `-Apply` 才写入账号同步服务器，之后由 Quicker 同步拉取；`-MetadataOnly` 仅同步外观和右键菜单，适合新包发布前改名。新草稿仍使用原生 MCP；V1 保留 `install-quicker-action.ps1 -Backend V1 -WorkspaceRoot <工作区>` 适配。

更新已有动作默认保留用户当前图标；显式传入 `-UpdateIcon` 才替换为动作源的图标。新建动作直接使用动作源图标。

Release 默认由 [GitHub Actions](../.github/workflows/release.yml) 构建：手动运行填写 `package_version` 后生成可下载的便携包产物；推送对应版本标签才创建 GitHub Release。CI 要求标签／输入版本与根 Cargo 版本一致，新建 Release 优先采用 `CHANGELOG.md` 对应版本的小节。当前开发与发版统一使用私有 `origin`（`ceastld/raishot`），归档备份只读，不更改仓库可见性。本机日常只做 Dev 增量构建，不因提交而等待 Release。

Quicker 依赖发布从已有 Release 产物开始，使用 `Publish-Dependency.ps1 -PackageVersion <新版本>`；默认只准备和完整核验包，`-Upload` 上传私有依赖，`-Upload -Public` 上传公开依赖。同版本内容不同会停止，不覆盖远程文件。动作账号更新、公开分享与 GitHub Release 分开执行。

离线验证：`pwsh ./quicker/Test-Action.ps1`。架构、命令和公开前待办统一见 [Quicker 动作与发布专题](../private/references/quicker-action-publication-2026-10-15.md)。

首个公开版本为 RabShot **0.4.14**；该版本的[更新说明](release-notes/0.4.14.md)保留发布时内容，[动作说明源](SHARE.md)已为下次 RaiShot 发布改名。已发布程序、依赖与图标的回读结论仅适用于原版本，仍需继续观察长时间闲置后的触发和跨账号完整安装。

## 历史分发记录

以下内容保留各次分发时点的说明；版本选择、维护与发布入口以上文为准。

## 2026-10-09：动作标题与图标优化

本轮仅更新本机 V1/V2 动作外观元数据：标题统一为“RustShot 截图”，采用蓝色实心裁剪图标 `fa:Solid_CropAlt:#3B82F6`，并同步创建源。程序正文、启动流程与 `0.4.8` 依赖包不变，未发布动作；本轮保存、导出及历史保护范围见[外观更新回执](../dist/quicker/action-appearance.json)。原 `0.4.8` 发布记录与封存保持原样。

## 2026-10-09：0.4.8 依赖包已发布，本机 V1/V2 动作已更新

按用户本轮授权，仅发布 `cea.rustshot / 0.4.8` 依赖包并更新已有本机动作，未分享或发布动作。包采用共同 Release **EE794696／604**，包含 Rust 进程内 OCR、Bitiful 独立依赖下载与多屏父选区即时反馈修复；[RustShot.0.4.8.0.zip](../dist/quicker/RustShot.0.4.8.0.zip) 为 **9,678,759 字节**，SHA256 `18EA2F56F35869A3FC1581EEAEE140CE61702731B0B94E222FA5D5B094290CA1`。私有上传成功，新的精确版本查询与独立完整匿名 GET 200 的 bytes／SHA 一致，见[最终分发回执](../dist/quicker/distribution-verification.json)及[0.4.8 封存](../dist/quicker/history/0.4.8/distribution-verification.json)。程序运行版本仍为 `0.4.0`，分发版本单独记录。

V2 已通过原生 MCP 保存并应用到原本机动作，正常 editVersion 为 `1791636987399`；实际从不存在的本版缓存首次下载六文件，`verify` 成功且输出排空。V1 已更新原本机动作，正常 editVersion 为 `1791637013477`，实际 CLI 自然 0、双流排空及 27 事件 trace 均通过。两版正常区域截图条件保留，验证均跳过 EXE。V2 首次因旧动作仅有 `assign1.txt`、缺少新的 `validate-package.value.txt` 而失败，回执保留；补建规范校验文件后在同一暂存区保存成功，不记为首次安装通过。

正式根目录 ZIP、完整 `.v1.json`、`.qka`、V2 源与本机收据已更新，旧根文件备份在 `candidates/0.4.8/previous-distribution/`；旧 0.4.7 候选、0.4.3 固定包和全部历史回执保留。OCR 五件运行依赖仍由 EXE 自行从 Bitiful 下载、校验与缓存，不借 Quicker 下载。未操作在用 RustShot 进程，使用新版须正常退出旧托盘后重新启动；异机、实际截图 GUI、当前加载与约 200ms 仍待验，不要求桌面空闲。以下“当前／未上传”等状态属于各自历史时点。

## 2026-10-09：0.4.7 更新候选，V1/V2 校验通过

最新动作候选为 [0.4.7 审阅说明](../dist/quicker/candidates/0.4.7/ACTION-REVIEW.md)，采用已完成的共同 Release **ECFA05C1／603**，包含调色盘修复与 Rust 进程内 PP-OCR／独立文字检测。ZIP **9,676,805 字节**，SHA256 `6B909634A9119CB094AF2F240573B96EAD98A1174491911D9A3A54F564517164`。仅调色盘的 0.4.6 是先前本地封存候选，不上传或作为本次更新版本；0.4.3 正式分发和 0.4.5 固定候选保留。

V1 的完整 [RustShot.v1.json](../dist/quicker/candidates/0.4.7/RustShot.v1.json) 和 [RustShot.qka](../dist/quicker/candidates/0.4.7/RustShot.qka) 与本机实际动作导出的正文一致；V2 源使用原生 MCP 暂存区保存。两版继续共用 `dependencycheck → assign → simpleIf → run`，校验用 `input.file`／`$=`，没有插件 DLL、MCP 或 RPC 的目标电脑依赖。安装脚本支持 `-SourceDirectory dist/quicker/candidates/0.4.7/action` 和两版 `-VerifyOnly -OutputDirectory target/qa/...`。当前 V2 已支持 `quicker_run` 的 `param`，直接传 `verify`，不修改正常启动条件，也不在 save 后重复 overwrite。

两版本机实际 `verify` 均通过：V1 为自然 0、27 事件 trace，V2 为成功且输出排空；只预置本版缓存、校验文件和跳过 EXE，未启动截图、重启在用进程或占用前台。上传预检通过并明确为 private，服务器查询暂未找到 0.4.7；尚未上传、替换原正式动作或推广根目录导入文件，异机首下载和实际截图仍待验。全部最终绑定见 [action-review.json](../dist/quicker/candidates/0.4.7/action-review.json)。使用新包前须正常退出旧 RustShot 托盘，否则新入口仍可能复用旧进程。

## 历史：2026-10-07 E2 与固定 BFE 的 0.4.5 候选

本机当前为E2/551待命禁用窗口候选，见1245交付及1258 Normal。待命保护专项、GPUI九宫格14项和光标47检查/54观测通过；Normal两次240.6468/191.6839ms仍未达100ms，不作稳定改善结论。当前常驻275776/HWND38214646，Trace/策略旗清空；详见[开发计划](../开发计划.md)。以下C63/BFE为旧记录，UIA/Pin实际范围仍是BFE；固定0.4.3和BFE的0.4.5 ZIP/动作均不覆盖，后者继续等待原上传授权。

此前交付C63/551首绘候选，见1150交付和1202 Normal；新源九宫格真实GPUI14项通过。首轮短检被系统通知遮挡；自有指针预置后，完整框选/取消/自然退出通过，Normal单次194.1611ms仍未达100ms，光标48检查/55观测通过；详细记录与后续计划见[开发计划](../开发计划.md)。下文BFE仍是固定0.4.5包及上一轮实际验证来源，不能自动扩大为C63重新通过；0.4.5包和动作审阅文件保持不变。

本机已交付 **BFE92D722434AD2C7F718EACE27D66F0DBE0854EA39940D339084FF222E9CF5E /551项**，见 1048交付。同源格式、严格Clippy、14模型与Release通过；九宫格14项、UIA10项及完成记录与贴图真实交接通过。贴图单次可见就绪116.441ms、两处内容采样完成133.9717ms；不是稳定或因果改善结论。

Normal短检为198.0668ms、保守201.7128ms，功能正确、原worker自然退出0，严格100ms仍失败；仅Count1/Idle0/Burst1。光标专项49检查/56观测无Busy，不覆盖硬件或所有位置。UIA仅精确单leaf正文，其余范围回退冻结底图OCR；自动复制偏好默认false，真实Clipboard、早期Native UIA与原生八方向仍未验。

本地固定候选 [RustShot.0.4.5.0.zip](../dist/quicker/candidates/0.4.5/RustShot.0.4.5.0.zip)已生成，**7,841,159字节**，SHA256 `277B6E2646F7B5ED50368FE888549443B74CF321465645CEDC69641F158F4CC1`；本机六文件及PE、[V1/V2审阅源](../dist/quicker/candidates/0.4.5/ACTION-REVIEW.md)与[共有步骤静态校验](../dist/quicker/candidates/0.4.5/action-review.json)完成。等待本版上传授权，0.4.5尚未上传、实际运行动作或异机下载。0.4.4本机ZIP说明过时，已废弃不分发。下方固定0.4.3、现场正常动作及封存文件不变，不包含BFE改动。

## 已封存 0.4.3

`cea.rustshot / 0.4.3` 已按用户授权上传，并完成 V1/V2 本机依赖及校验流程验证，正式导入文件与 ZIP 已推广到 `dist/quicker/`。程序运行版本仍为 `0.4.0`；固定包来源为 **731A1ACA97033F6247097DBD2320AFC8FF1983324520C9E3F6536CF2BC964EB8 /545 项**。包为 [RustShot.0.4.3.0.zip](../dist/quicker/RustShot.0.4.3.0.zip)，**7,726,049 字节**，SHA256：`37B8EA597297C902DC407C6071F4E2BE4E9DADD023215DBD68EE80E71C1B9C56`；完整分发封存汇总见 [distribution-verification.json](../dist/quicker/history/0.4.3/distribution-verification.json)，原候选证明 [candidate-verification.json](../dist/quicker/candidate-0.4.3/candidate-verification.json)仍保留。

- 上传及完整 GET 回执通过：上传 CLI 成功，服务器包 ID 为 `43ba7ce1-83b5-4bd7-e517-08df23baf840`，新的完整 GET 与固定 ZIP 大小及 SHA256 相同。没有覆盖 `0.4.2` 或公开发布动作。
- V2 本机校验回执通过：本版缓存原先不存在，由依赖步骤重建六个文件并核对完整清单及哈希；EXE 启动被跳过，输出排空，正常源已恢复。自有动作 `bacf6bb1-e6a1-468b-8b5d-22f8b28c80d9`，slot `65sy`，正常草稿版本 `13`。这是本机缺缓存重建，动作自身的 HTTP 请求未独立观测；上面的完整 GET 是单独验证。
- V1 完成校验回执通过：复核原八个 CLI 的原 HANDLE 自然退出 0、双流排空及 27 个 trace 事件，确认依赖校验和跳过 EXE；另作五次只读 GET/export 核对当前及最终完整正常源，未再次 apply 或运行动作。自有动作 `69a62214-ef8a-4e2a-9dff-8bfe5d91a9fc` 的正常 editVersion 为 `1791334853968`。原 V1 父回执因 runner 漏取 `taskCondition` 仍为失败原件，以新完成回执说明已核实的范围。
- 正式完整导入文件为 [RustShot.v1.json](../dist/quicker/RustShot.v1.json)（6,017 字节），V1 设计器正文为 [RustShot.qka](../dist/quicker/RustShot.qka)（4,749 字节）；V2 正常版本为 `13`，本机收据仍只供维护，不能当作动作导入文件。本地推广回执确认当前文件和 `history/0.4.3/` 总封存已完成，旧版及原失败回执保持原样，未再次运行动作。正常现场兼容导出另见 V1 完成校验输出。

上述结果不是异机首下载、异机运行或截图 UI 验收。731A 的一次短 Normal 为 NativePost→真实 Ready **217.4077ms**、保守 **217.5304ms**、request→Ready **170.4781ms**，功能正确且原 worker 自然退出 0，100ms 仍未达标。ZIP 内 `README.txt` 固定于封存前的准备阶段，里面的“未上传”状态由本节及后续原始回执更新说明；已上传 ZIP 及其包内说明保持不变。工作树 `PORTABLE-README.txt` 已为新的0.4.5候选更新。固定 `0.4.2` 和 `0.4.3` 均不可覆盖，后续构建内容变化必须使用新的包版本。

历史后续源码 **3D775E87F8BE04C63BB669FA526EFACE55ED110F9EBD79FCFD3514443768FF14 /545 项**，包含首屏绘制分段 Trace、单次完整日志写入和待命标准图标复制后立即释放的预热；Release 与 本机交付通过。一次正常模式测量为 NativePost→真实 Ready **220.7883ms**，功能正确、自然退出 0，严格 100ms 门仍失败。光标专项完成 49 项检查及 58 次观测，没有观察到 WAIT/APPSTARTING；这不扩大为所有控件或硬件输入验收。Trace 串行分支仅用于诊断，已恢复正常模式。这些后续改动均不在已上传的固定 `0.4.3` 包内。

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

沿用 Workspace 的 Quicker 官方依赖上传通道。`qkbuild` 的 EXE 构建步骤依然要求 csproj，因此这里只复用下载/上传约定，Rust 构建保持本项目自己的入口。

```powershell
# 仅预检，不上传：
quicker dep upload --file dist/quicker/RustShot.0.4.2.0.zip --name cea.rustshot --version 0.4.2 --description "RustShot Windows x64 便携测试包" --dry-run

# 经用户授权上传新版本；本版 0.4.2 已完成，保留命令供参考：
quicker dep upload --file dist/quicker/RustShot.0.4.2.0.zip --name cea.rustshot --version 0.4.2 --description "RustShot Windows x64 便携测试包"
quicker dep find --name cea.rustshot --version 0.4.2
```

默认上传表单的 `IsPublic=False`，测试计划使用登录同一 Quicker 账号的电脑；包查询携带账号授权且不绑定设备。本版已按用户授权上传依赖，未发布到公开动作库。已从服务器返回的文件 URL 完整下载 ZIP，且该次 GET 未附账号 Authorization 请求头；`IsPublic=False` 不能当作仅作者可下载的保密保证。本机缺失缓存重建已验证；跨账号权限、该依赖步骤的独立 HTTP 观测及异机下载运行尚未验证。

Quicker 缓存位于系统“文档”目录的 `Quicker/_packages/cea.rustshot/<三段版本>/`。依赖步骤使用最低版本，并优先选择已有的较新版本；动作检查清单的包名、架构、版本与每个必需文件的大小及 SHA256，避免缺件或文件混用。这是完整性检查，不是发布者数字签名。不要覆盖同版远程包来期待所有电脑刷新缓存。

维护用 `verify` 参数只下载/检查依赖，不启动截图。当前 V1/V2 的 `-VerifyOnly -OutputDirectory target/qa/...` 均传 `verify` 并检查实际执行日志；正常源保留。早期 V2 没有调用参数时使用临时 false 条件的记录仅属历史。开发机的本地预置缓存验证不能冒充联网首下载或异机结果。

## 历史 0.4.2 分发与验证

以下“当前/本轮”仅描述已封存的 `0.4.2` 阶段；当前 `0.4.3` 状态见顶部，旧版文件和回执保留。

固定 `0.4.2` 包的汇总证明入口为 [distribution-verification.json](../dist/quicker/history/0.4.2/distribution-verification.json)，SHA256 为 `F9DECCA5E8E585D84F27B52C49C0CC635F11CA76B87D5143F1592EDB933DD221`。它绑定下列原始收据；后续工作树改动不包含在这个固定包中。

- 当前包为 `cea.rustshot` / `0.4.2`，程序运行版本为 `0.4.0`。`dist/quicker/RustShot.0.4.2.0.zip` 为 **7,704,404 字节**，SHA256 为 `FE3D08EEE3D438929A44F609649365878F50883B7CD29620FB83FB6BC0991D43`。六个根目录条目、清单和文件大小及哈希已核对，三件套与已安装的同源 Release 一致；来源清单 545 项，SHA256 为 `1D6A2484BE69D62C8AC1A7BA2DE1CEEA817D3DFAE7B482AD053308F64D6CC47C`。ZIP 和包内说明冻结于上传前，上传后的分发状态以本节和收据为准。
- 当前 V1/V2 声明均要求 `0.4.2`，共用包根 `rustshot.exe` 和相同清单校验。自有 V2 暂存动作 `bacf6bb1-e6a1-468b-8b5d-22f8b28c80d9` 位于 slot `65sy`，正常草稿版本 `11`；V1 兼容动作 `69a62214-ef8a-4e2a-9dff-8bfe5d91a9fc` 的正常 editVersion 为 `1791323043012`，名称均为“RustShot 便携测试”。正常条件的完整 `RustShot.v1.json`、`RustShot.qka`、V2 源副本与本机收据已更新到 `dist/quicker/`。副本及推广收据在 `dist/quicker/history/0.4.2/`；推广收据只证明正常导出文件已复制，其引用的旧整轮 QA 仍为失败原件。
- 本机 `Quicker/_packages/cea.rustshot/0.4.2` 以新目录和逐文件 `CreateNew` 方式预置，未覆盖旧版本；缓存、ZIP、清单和 Release 三件套一致。缓存收据为 `target/qa/quicker-cache-0.4.2-20261006-073619-935.json`。在此缓存上，V1 的 `verify` trace 确认依赖查找、`sys:assign` 和跳过 EXE 启动，原 qkrpc 进程自然退出 0，stdout/stderr 排空；最终动作读取仍是正常框选条件。
- 最终动作校验收据为 `target/qa/quicker-distribution-0.4.2-resume-cli/20261006-080818-958-runtime-1830c9/verification.json`，`ok=true`。本轮执行 7 次 V2 原生只读请求和 5 次 V1 请求；V2 VerifyOnly 运行及恢复版本 `11` 的证据复用此前已完成的同动作记录，最终 `running=false`、`hasMore=false`，并重新核对当前正常源。本轮未再次运行或保存 V2。两版均跳过包中 EXE；这是动作依赖与校验流程验收，未作为实际截图 UI 或延迟测试。
- 随后的首次本机缓存重建收据为 `target/qa/quicker-distribution-0.4.2-first-download/20261006-082217-873-runtime-8199be/verification.json`，`ok=true`。精确的原 `0.4.2` 缓存先移至该输出目录的 `new-owned-preseed-backup` 并确认原路径不存在，再由 V1 `sys:dependencycheck` 重建完整六文件；全部大小、SHA256 和清单均核对通过。V1 正常 editVersion 仍为 `1791323043012`，V2 正常版本 `11` 仅只读核对并复用此前运行证据，没有启动 EXE。原缓存备份保留，未覆盖或清理。收据的 `remote_first_download=true` 仅指本机从无本版缓存到依赖步骤重建完整缓存；该步骤的 HTTP 请求未独立观测，异机首下载及运行尚未验证。
- `0.4.2` 预检后已按用户授权上传固定 ZIP，上传 CLI 退出 0；服务器查询确认包 ID `703a75a8-fcab-4ad5-e516-08df23baf840`、包名、版本及大小。随后对服务器 URL 发起新的完整 HTTP GET，下载 **7,704,404 字节**，SHA256 与上述 ZIP 完全相同。上传记录在 `target/qa/quicker-upload-0.4.2-20261006-080316-334/`，查询与完整下载证明在 `target/qa/quicker-remote-download-0.4.2-20261006-080708-036/verification.json`。这是独立完整 GET 校验；上述 Quicker 步骤的缓存重建另有收据，二者不扩大为异机测试。
- 包采用的 `1D6A/545` 来源已通过同源格式、严格 Clippy、Release、自有合成多屏 headless 和只读待命元数据检查。本机实际光标矩阵 43 项检查通过，79 条 observations 中有 76 个去重光标采样，未观察到 WAIT/APPSTARTING；三个自有 GPUI 窗口物理点跨两个显示器，每点 3 个样本，恢复原位也采样 3 次，第一点包含 NONE→CROSS 过渡。范围不覆盖硬件鼠标、真实工具栏按钮或自绘光标像素。一次短 Normal 样本为 NativePost→真实 Ready **195.3244ms**、保守 Send→Ready **195.486ms**、request→Ready **154.04ms**；框选 ACK 正确且原工作进程自然退出 0，100ms 门仍失败，无五轮、60 秒待命和最后 8 次突发验收。常驻与待命工作进程的一次 private bytes 采样合计 3,928,064 字节，连续峰值及系统 DWM/GPU 占用未测。范围与来源审计为 `target/qa/startup-device-dpi-final-audit/20261006-142853-431-442fdf/verification.json`。

### 历史 0.4.1 与更早版本

以下“当前/本轮”仅描述历史 0.4.1 阶段。其兼容文件、源副本与收据另存于 `dist/quicker/history/0.4.1/`，归档证明为 `archive-verification.json`；原 0.4.1 ZIP 仍保留在 `dist/quicker/`。

- 当前包：`cea.rustshot` / `0.4.1`，ZIP 为 `RustShot.0.4.1.0.zip`，7,647,498 字节，SHA256 为 `82665fd7fcbc451e7b40a6fa9168a4e9c286762a026041a424981aa646b00cb9`。包内所有条目的大小与清单哈希已核对，三件套与 2026-10-06 成功 Release 相同（来源清单 545 项，SHA256 `27D82F1ADA1E05232569FE3FCFC6AFEED4D35FEE26959157F83C1A3AD2781F8F`）；PS5.1 同版本重用成功且 ZIP 哈希未变。包内说明不嵌入 ZIP 自身指纹，以免产生自指哈希。
- 当前 V1/V2 声明和共有校验文件使用包根目录的 `rustshot.exe`，版本要求为 `0.4.1`。已同步自有 V2 暂存动作 `bacf6bb1-e6a1-468b-8b5d-22f8b28c80d9`（slot `65sy`，正常草稿版本 `7`）与 V1 本地兼容动作 `69a62214-ef8a-4e2a-9dff-8bfe5d91a9fc`，名称均为“RustShot 便携测试”。当前 `dist/quicker/RustShot.v1.json` 和 `RustShot.qka` 已重新导出；V2 收据 `RustShot.v2-local.json` 和源副本仅供维护。历史兼容文件与收据已保存在 `dist/quicker/history/0.4.0/`。
- 本轮只新建了 `Quicker/_packages/cea.rustshot/0.4.1` 本机缓存，未覆盖既有缓存。V1 实际传入 `verify`，V2 暂时保存精确的 false 启动条件；两版依赖检查和 `sys:assign` 均通过，日志确认跳过启动。V2 输出已排空至 `running=false`、`hasMore=false`，随后恢复本仓正常框选条件并保存版本 `7`。收据与原始日志在 `target/qa/quicker-distribution-0.4.1-20261006-0940/`。这是本机预置缓存验证，未触发包中 EXE，也不代表联网首下载或异机运行。
- 同一 Release 的独立实际光标 QA 在无 trace 环境通过 47 项检查，所有样本无 WAIT/APPSTARTING，原工作进程正常退出 0。范围包括原生 Ready、held drag、GPUI 交接、原生捕获时的光标切换和自有前台窗口保护；角点 None 只按现有 Move/Diagonal 自绘语义接纳，未验证覆盖层像素或全部标注 UI。证据为 `target/qa/owned-detach-startup-cursor/20261006-091923-769-a3dbf1/verification.json`。正常首屏延迟约 262ms，100ms 目标仍未达到；resident-only 诊断样本不作正常验收。
- 历史 `0.4.0` ZIP 为 7,611,196 字节，SHA256 为 `18e8a39df0e4a833af3252133480b754a2d7733a852e50bcc33689ddb98e4a8e`；此记录仅标识旧包。
- 历史 `0.4.0` 包解压到 `target/qa` 后，从其它工作目录运行原生首屏 smoke，5 个合成夹具场景通过；实际加载解压目录的 `VCRUNTIME140.dll`，worker 正常退出。此结果不自动覆盖本轮 `0.4.2`。没有将真实桌面截图加入包。
- 历史预置的 `Quicker/_packages/cea.rustshot/0.4.0` 本机缓存验证保留。改用共有 `sys:assign` 后，V1 trace 与 V2 原生 MCP 运行均完成依赖查找、文件校验及跳过启动；V2 已确认 `running=false` 且 `hasMore=false`，随后恢复默认框选条件并保存/打开预览。没有把检查运行当成真实截图 UI 验收，也尚未联网首下载或异机运行。
- 旧 `sys:evalexpression` 在 V2 注册表未启用，已移除该依赖。校验坏哈希时按预期拒绝，之后文件可独占读取，句柄已释放；同一校验主体的结果保留。使用显式 `try/finally`；`verify` 使用共有的 `sys:simpleIf`。
- 历史 `0.4.0` 上传 `--dry-run` 通过；本次 `0.4.2` 的固定 ZIP 预检、上传、查询和完整服务器下载记录见上文。历史预检不计作当前上传结果。

历史包与 GPUI 证据在 `target/qa/quicker-distribution/8d8f1aa8a1d5477f87de1ed5d6abff58/`；共有步骤与 V2 原生验证保存在 `target/qa/quicker-v1-v2/`，另有 `target/qa/expr-package-audit-a2/` 的历史校验诊断。它们不能代替本轮最终包、当前动作入口和真实异机首下载的验证记录。

当前托盘通知到真实冻结桌面可框选的 100ms 目标尚未达成；分发包版本增加不代表达到性能目标。具体交付版本和测量边界见根目录 `继续迭代.md`。VC++ 同目录部署参见 [微软说明](https://learn.microsoft.com/en-us/cpp/windows/redistributing-visual-cpp-files?view=msvc-170)。
