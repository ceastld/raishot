# RustShot 测试台

本机 React + Vite 测试界面，用来放需要真实调用或反复对比的测试页，并统一查看 `docs/design/` 下的 HTML 原型。开发上下文与设计原型随源码公开保存在 `docs/`；查看原型不需要维护者的本机配置，调用真实服务时按需填写自己的接口设置。

```bash
npm --prefix Tools/workbench install
npm --prefix Tools/workbench run dev   # http://127.0.0.1:5178
```

- **翻译测速**（`#/translate`）：真实调用翻译接口，对比整段等待／流式／分段并发。提示词、参数、空流重发与代理选择同 `src/translate.rs` 和 `download_agent`；开发服务器的 `/relay` 转发请求并逐块回传（绕过浏览器跨域）。配置与历史存在本机浏览器 localStorage，可复制 JSON。
- **录屏编解码**（`#/codec`）：读 `target/qa/codec-lab/` 下 `recording_quality_lab` 的输出（`RUSTSHOT_LAB_SOURCE=png cargo test --lib recording_quality_lab -- --ignored`），表格对比码率／PSNR，画面按物理像素 1:1 做滑动、并排、闪切、误差对比，并可播放各方案 MP4。
- **设计原型**：侧栏自动列出 `docs/design/*.html`，新增原型无需登记，浏览地址仍为 `/design/<文件名>`。[操作地图](../../docs/design/interaction-guide.html) 汇总各场景快捷键、可点击入口、保存流向与本地数据位置，支持搜索和同键反查，也可直接用浏览器离线打开。目录缺失或尚无 HTML 时列表为空，其余测试页照常使用。
- **新增测试页**：在 `src/tools/` 写组件，在 `src/App.jsx` 的 `TOOLS` 里加一行。需要服务端能力时在 `vite.config.js` 的 `workbenchServer` 里加中间件。
