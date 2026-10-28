# Release CI 构建速度（2026-10-19）

目标：在标准 Windows runner 上显著缩短 Release 构建，并实际检查产物；本机继续使用 Dev，不做本地 Release 编译。

## 基线

[首轮云端构建](https://github.com/ceastld/RabShot/actions/runs/39226501652)成功：Cargo 32 分钟，整个任务 34 分 11 秒；此前没有依赖缓存。准备运行库、打包和上传均只需数秒，时间主要在编译与链接。

旧配置为 `lto = "thin"`、`codegen-units = 1`、两个并发 cargo 工作任务。三个 EXE 共享大体积 `rustshot` 库，跨库 LTO 会在最终产物链接时再次参与优化；仅凭编译开始日志不能准确拆分每个库和链接的耗时。

## 改动

- Release 改为 `lto = "off"`、`codegen-units = 16`，保留主库 `opt-level = 2`、GIF 热点 `opt-level = 3`、符号裁剪与 panic abort。关闭跨库优化可能增加包体、减少部分运行优化，不能把编译提速等同于运行性能完全一致。
- 保留 `rust-cache` 的外部依赖产物缓存，失败时也保存已完成的依赖。其默认筛选按包路径排除仓库内的库，因此不能保证保留本地 GPUI、codec；叠加 sccache 的内容缓存复用这些库，避免 checkout 时间变化导致重复 LLVM 编译。编译缓存只用于 CI，不随软件分发。
- `build.ps1 -Timings` 输出 Cargo HTML 耗时报告；CI 始终保存报告并打印 sccache 命中统计、构建秒数。缓存参数只放在编译步骤，避免无谓改变已有 Cargo 缓存键。
- CI 检查 Release CLI 能正常启动；便携 ZIP 继续执行原有必需文件、固定运行库与清单校验。
- 固定 ONNX Runtime 四个成员单独缓存，复用前逐项检查 bytes／SHA256；缓存未就绪时使用 Microsoft 官方固定版本整包（74,442,783 字节），只解出所需的四个成员。当前清单的主源与 `upstream_*` 均为同一官方包，不提供独立镜像备用地址；运行库成员校验不变。

依据：[Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)、[Cargo timings](https://doc.rust-lang.org/cargo/reference/timings.html)、[rust-cache 路径筛选](https://github.com/Swatinem/rust-cache/blob/v2/src/workspace.ts)、[sccache Rust 缓存限制](https://github.com/mozilla/sccache/blob/v0.16.0/docs/Rust.md)。

## 实测

下列数据记录 2026-10-19 的历史 CI 配置与网络情况；表中的旧镜像等待不表示当前仍使用镜像，也不代表清单调整后的重新实测。

| 配置 | Cargo 构建 | 整个任务 | 结果 |
| --- | --- | --- | --- |
| 旧配置，首次无缓存 | 32 分钟 | 34 分 11 秒 | 成功 |
| [新配置，首次填充缓存](https://github.com/ceastld/RabShot/actions/runs/39231604231) | 16 分 8 秒 | 20 分 11 秒 | 成功；431 次缓存未命中，写入错误为 0 |
| [新配置，同一提交复用缓存](https://github.com/ceastld/RabShot/actions/runs/39234290789) | 50.48 秒 | 5 分 16 秒 | 成功；旧镜像下载超时占约 3 分钟 |
| [修正运行库下载，首次缓存 DLL](https://github.com/ceastld/RabShot/actions/runs/39235026638) | 1 分 25 秒 | 3 分 29 秒 | 成功；官方源准备运行库仅 3 秒 |
| [编译与运行库均复用缓存](https://github.com/ceastld/RabShot/actions/runs/39235618193) | 1 分 42 秒 | 3 分 46 秒 | 成功；运行库校验复用，编译缓存 3 命中／0 未命中 |

新配置首轮 Cargo 报告：主库 355.94 秒、GPUI 47.33 秒、codec 2.34 秒，三个 EXE 分别 1.51／3.94／3.46 秒。与基线相比，完整编译约缩短一半，整轮任务缩短约 41%；首次还需编译不同参数的依赖并保存约 788MB Cargo 缓存。

最终便携 ZIP 为 20,329,973 字节，基线为 22,524,874 字节，缩小 9.7%。下载后核对 ZIP SHA256 与清单中全部 14 个文件的 bytes／SHA256，通过；CI 的 CLI 启动、打包和上传均成功。

三次缓存复跑均为 3 次编译缓存命中、0 次未命中、0 次写入错误；标准 runner 的缓存读取与执行时间有波动，实测编译为 50–102 秒。修正运行库下载后的完整 CI 为 3 分 29 秒／3 分 46 秒，相比最初冷构建约快 9–10 倍。

首次构建对比反映配置调整，缓存复跑另外体现缓存收益；首轮与新主分支之间包含后续功能提交，不能称为相同源码的严格 A/B。复跑间 Rust 编译输入相同；修改主库源码后仍需重编，不能把同源码复跑耗时当作所有代码改动后的耗时。真实界面与运行性能专项未验。
