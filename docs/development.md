# 开发

本文约定源码构建与验证方式。环境要求和基础构建命令见 [README](../README.md#build-and-contribute)，贡献授权与提交要求见[参与贡献](../CONTRIBUTING.md)。

## 环境与依赖

开发目标为 Windows x64，使用 Rust 1.90 及以上、MSVC C++ 工具链与 Windows SDK。GPUI 使用仓库锁定的依赖；修改相关补丁前阅读[依赖说明](../vendor/README.md)。构建和默认测试应独立于仓库外的目录，也不依赖维护者的本机配置、研究样本或历史测试产物。

## 验证

先检查编译，再按受影响模块选择测试：

```powershell
cargo check --locked
cargo test --locked --lib <过滤词>
```

过滤词应覆盖改动涉及的行为与边界。需要运行程序时再构建：

```powershell
cargo build --locked
```

日常开发默认使用 Dev，产物位于 `target/debug`；发布构建由 CI 负责。验证只做证明改动所需的范围，相关代码没有变化时不重复已经通过的检查。PR 中写明实际验证范围和仍未验证的交互。

## 格式化

只格式化本次修改的 Rust 文件：

```powershell
rustfmt --edition 2021 <文件>
```

避免整仓格式化带来无关改动。

## 工作区与运行

开始修改前检查 `git status --short` 和目标文件的差异，保留已有改动。增量编译复用当前工作区的 `target/`；同一工作区的 Cargo 构建串行执行。

运行时保持 `raishot.exe`、`raishot-gpui.exe`、`raishot-cli.exe` 位于同一目录。测试或预览窗口跟随 Windows 主题；界面验证应使用与改动相关的操作和数据。

## 文档

修改前按[架构](architecture.md)确认模型与界面边界。行为、快捷键或长期约束变化时更新所属文档，概念含义以[术语](glossary.md)为准。
