# 验包与发布

Windows CI 在向 `main` 推送或提交 PR 时执行全目标 Clippy 编译检查与核心单元测试；仅 Markdown 或 `docs/` 的修改跳过这些编译任务。Release 工作流复用这些检查，通过后才构建和打包。发布构建由 GitHub Actions 完成，不需要在本机编译 Release。

工作流定义见 [Release](../.github/workflows/release.yml)，开发时的定向验证见[开发](development.md)。

## 手动验包

在仓库目录中使用已登录的 GitHub CLI：

```powershell
gh workflow run release.yml --ref main
```

也可在 GitHub 的 Actions → Release → Run workflow 中选择 `main`。`package_version` 可以留空，默认读取所选源码的 `Cargo.toml` 产品版本；显式填写时必须与产品版本一致。

手动运行只生成 Actions artifact，不创建或更新 GitHub Release，也不创建标签。当前源码中的 Cargo 版本可能与已发布的最新版本不同；验包仍按源码版本命名，不能据此重用已有发布标签。

运行成功后，从该次运行下载 RaiShot 分发 artifact，解压 artifact 后取得便携 ZIP 与对应的 `.sha256` 文件。核对 ZIP 的 SHA-256，检查包内三个 EXE、VC++ 与 ONNX Runtime 文件、项目和第三方许可材料及 `manifest.json`；实际运行时保持三个 EXE 的名称和相对位置不变。需要验证界面或媒体行为时，在合适的真实 Windows 桌面上使用这份包。

托管 CI 的编译、测试与 CLI 启动检查不等于 GUI／GPU 验收；真实桌面交互、HDR、混合 DPI 和音视频设备等范围仍需按本次改动另行记录。

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

标签触发 Release 工作流。它先确认标签版本、Cargo 产品版本一致，且 changelog 中存在对应条目，再执行 CI 验证、Release 构建和打包。所有材料通过后创建 Release 草稿，上传便携包与校验文件，最后将草稿发布。

## 失败与重跑

先查看失败步骤的日志，再重跑失败的任务。标签和源码相同的失败运行可以重跑；工作流会校验已有同名资产的大小与 SHA-256，一致时复用，不一致时停止。草稿缺少的资产可以补传；已公开版本缺少资产时停止，不能覆盖或补改已发布内容。上传中断留下的草稿只在材料完整且校验通过后发布。

修复源码或改变包内容后，使用新的版本和标签；不要移动、删除重建或重用旧发布标签，也不要手工替换已发布的同名包来掩盖差异。

工作流使用 GitHub 自动提供的 `GITHUB_TOKEN`，发布任务声明所需的 `contents: write` 权限，无需另建 PAT 或把个人令牌保存为发布 Secret。本地执行 `gh workflow run` 则使用 GitHub CLI 当前登录账号的仓库权限。
