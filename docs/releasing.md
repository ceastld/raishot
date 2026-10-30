# 验包与发布

Windows CI 在向 `main` 推送或提交 PR 时执行全目标编译检查与核心单元测试；仅 Markdown 或 `docs/` 的修改跳过这些编译任务。两项检查都是必过门槛；Clippy 另行输出完整报告，当前的存量告警暂不阻断，失败时会在运行摘要中标明。

Release 工作流并行执行质量检查和 Release 构建打包，两边成功才允许发布。发布构建由 GitHub Actions 完成，不需要在本机编译 Release。

工作流定义见 [Release](../.github/workflows/release.yml)，开发时的定向验证见[开发](development.md)。

## 手动验包

在仓库目录中使用已登录的 GitHub CLI：

```powershell
gh workflow run release.yml --ref main
```

也可在 GitHub 的 Actions → Release → Run workflow 中选择 `main`。`package_version` 可以留空，默认读取所选源码的 `Cargo.toml` 产品版本；显式填写时必须与产品版本一致。

手动运行只生成 Actions artifact，不创建或更新 GitHub Release，也不创建标签。当前源码中的 Cargo 版本可能与已发布的最新版本不同；验包仍按源码版本命名，不能据此重用已有发布标签。

运行成功后，从该次运行下载 RaiShot 分发 artifact，解压后取得便携 ZIP、安装包及各自的 `.sha256` 文件。核对 ZIP 的 SHA-256，检查包内三个 EXE、VC++ 与 ONNX Runtime 文件、项目和第三方许可材料及 `manifest.json`；实际运行时保持三个 EXE 的名称和相对位置不变。需要验证界面或媒体行为时，在合适的真实 Windows 桌面上使用这份包。

托管 CI 的编译、测试与 CLI 启动检查不等于 GUI／GPU 验收；真实桌面交互、HDR、混合 DPI 和音视频设备等范围仍需按本次改动另行记录。

## 安装包

每次发版同时提供便携 ZIP 和安装包 `RaiShot-X.Y.Z-setup-x64.exe`。安装包由 [installer/raishot.iss](../installer/raishot.iss) 用 Inno Setup 编译。CI 会按固定版本下载 Inno Setup 并校验 SHA256，然后把便携 ZIP 重新展开作为安装文件，因此两种包的程序文件逐件相同。

| 项 | 约定 |
| --- | --- |
| 范围 | 只装给当前用户，不需要管理员权限；默认目录 `%LOCALAPPDATA%\Programs\RaiShot`。安装目录不能与数据目录 `%LOCALAPPDATA%\RaiShot` 重叠 |
| 入口 | 开始菜单快捷方式运行 `raishot.exe --tray`，桌面快捷方式可选；在 HKCU 的 App Paths 登记 `raishot.exe`；可选把安装目录加入用户 PATH，以便直接使用 `raishot-cli` |
| 开机启动 | 只在首次安装时提供，默认勾选。写入格式与 `src/autostart.rs` 相同；升级时不改动，以用户在设置中的选择为准 |
| 升级 | 固定 AppId，覆盖原目录。安装前用新包里的 `raishot.exe --exit` 请本会话托盘（安装版或便携版）正常退出，仍占用文件的进程交给 Restart Manager 处理 |
| 卸载 | 先 `--exit`，再删除程序、快捷方式、App Paths 和 PATH 项。只删除指向本安装目录的启动项。用户数据默认保留，交互卸载时可选择删除；静默卸载一律保留；“图片”中的截图不删 |

`raishot.exe --exit` 等同于托盘菜单“退出”，最多等待 20 秒。退出码 0 表示已退出或没有托盘在运行，1 表示超时，2 表示请求失败。

静默安装使用 `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART`，可用 `/TASKS="autostart,desktopicon,clipath"` 选择附加项，静默安装后不会自动启动托盘。CI 在临时目录完成一次静默安装和卸载，核对 CLI、启动项、App Paths 和 PATH 的写入与清理。

安装包暂未签名，首次运行可能出现 SmartScreen 提示。需要签名时，在“构建安装包”步骤之后对 EXE 签名，再生成校验文件。

## 准备正式版本

选择从未发布过的新版本。下面的 `X.Y.Z` 是占位符，操作前替换为本次版本：

1. 修改根 `Cargo.toml` 的 `[package].version`。
2. 同步 `Cargo.lock` 中 `name = "rustshot"` 对应的根包版本；不要全局替换其它依赖的版本号。
3. 将本次更新整理到 `CHANGELOG.md` 中对应的 `## X.Y.Z` 节，写明实际改动与必要的验证边界。
4. 按贡献要求提交并推送 `main`，确认该提交的 Windows CI 通过；按需完成手动验包。
5. 在准备发布的提交上创建并推送新标签：

```powershell
git tag vX.Y.Z
git push origin vX.Y.Z
```

标签触发 Release 工作流。它先确认标签版本、Cargo 产品版本一致，且 changelog 中存在对应条目，再并行执行 CI 验证与 Release 构建打包。两边通过后创建 Release 草稿，上传安装包、便携包及各自的校验文件，最后将草稿发布。

## 失败与重跑

先查看失败步骤的日志，再重跑失败的任务。标签和源码相同的失败运行可以重跑；工作流会校验已有同名资产的大小与 SHA-256，一致时复用，不一致时停止。草稿缺少的资产可以补传；已公开版本缺少资产时停止，不能覆盖或补改已发布内容。上传中断留下的草稿只在材料完整且校验通过后发布。

修复源码或改变包内容后，使用新的版本和标签；不要移动、删除重建或重用旧发布标签，也不要手工替换已发布的同名包来掩盖差异。

工作流使用 GitHub 自动提供的 `GITHUB_TOKEN`，发布任务声明所需的 `contents: write` 权限，无需另建 PAT 或把个人令牌保存为发布 Secret。本地执行 `gh workflow run` 则使用 GitHub CLI 当前登录账号的仓库权限。
