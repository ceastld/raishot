# 参与贡献

欢迎提交缺陷修复、功能改进和文档更新。功能与使用方法见 [README.zh-CN.md](README.zh-CN.md)。开发讨论与文档使用中文，界面文案提供中英文。

## 开发与验证

从 [docs/](docs/README.md) 阅读完整开发上下文：术语与架构、行为与路线、当前待办、技术专题和设计原型均随公开源码维护。

Windows x64 开发需要 Rust 1.90 及以上、MSVC C++ 工具链与 Windows SDK。使用仓库锁定的 `vendor/gpui`；修改补丁前阅读 [vendor/README.md](vendor/README.md)。构建和默认测试不依赖仓库外的任何目录。

先运行 `cargo check --locked`，按影响范围选择 `cargo test --locked --lib <过滤词>`。只对改动的 Rust 文件运行 `rustfmt --edition 2021 <文件>`。真实界面验证范围与未验项请在 PR 中说明。

## 贡献授权与签署

提交自有代码或文档贡献，表示你有权并同意按 [MIT 许可证](LICENSE) 授权该贡献，且理解提交记录及签署信息会公开保留。第三方文件继续遵守文件原有许可证。贡献者保留版权；MIT 允许维护者及其他接收者按许可条件使用、修改和商用。每条提交使用姓名与可联系的邮箱签署，确认贡献来源和上述授权：

```text
Signed-off-by: Your Name <you@example.com>
```

可使用 `git commit -s` 添加签署。许可范围见[许可说明](docs/license.md)。修改第三方代码时保留原许可证、版权与通知。不要提交密钥、用户截图、个人配置或未经许可的样本。

PR 描述写明问题、改后行为、必要的验证与未验范围；新增依赖或复制第三方代码时补充 [第三方通知](THIRD_PARTY_NOTICES.md)。
