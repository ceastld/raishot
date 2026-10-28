# 长截图宣传演示

2026-10-24：`docs/design/promo-long-capture.html` 是官网用的 1280×720 舞台动画，与 `promo-ocr.html` 同一套舞台、时钟、章节与 `?embed` 嵌入。区别在于拼接不是预录：页面把 `src/stitch.rs`、`stitch_stream.rs`、`stitch_content.rs`、`seam_alignment.rs`（含 `seam_scores` / `seam_motion`）原样编成 WebAssembly，每帧把浏览器窗口模型的像素交给生产 `Stitcher`，耗时、位移、拼接长度、固定栏都是实时结果。

## 构成

- `Tools/promo-wasm/`：独立小 crate（自带空 `[workspace]`，不进主工程、不依赖 GPUI），只补 `Image`、`Rect`、`Result`、文案宏，用 `#[path]` 引用生产源码并导出 C ABI。`build.ps1` 用 rustup 工具链编 `wasm32-unknown-unknown`（需 `rustup target add wasm32-unknown-unknown`），产物写到 `target/promo-wasm/`，再 base64 内嵌为 `docs/design/promo-stitch-wasm.js`，所以 `file://` 直接打开也能运行。改动上述拼接源码后重跑 `build.ps1`。
- `examples/bench.rs`：同一份代码的原生基准（程序生成带固定页眉页脚的长页面）。`cargo run --release --example bench --target-dir ../../target/promo-wasm`。
- 计时只包 `st_process`：像素拷入、页面绘制、预览回读不计，对应产品里采集线程交出帧之后的拼接本身。

## 章节

极速拼接（自动滚动 ×1→×10，拼完一帧再滚下一步）→ 固定栏识别（显示检测出的页眉/底栏，最后并排对比“按位移直接追加帧底部”与真实拼接）→ 运动追踪（变速、停顿、往回滚的视口回访，面板画实测位移与按生产规则算出的预测）→ 移动选区（页面静止、拖动选区续拼）→ 极限压测（300 帧随机位移，不等刷新连续拼接）→ 结尾。控制条“亲手试试”进入滚轮/键盘驱动的真实采集，工具栏可自动滚动、撤销、重来。

## 实测与口径

- 560×500 帧（页面所用尺寸）：浏览器内 wasm（SIMD）约 12–13 ms/帧，压测 300 帧约 80 帧/秒、每秒拼出约 9000 px；原生 x64 约 4 ms/帧。
- 原生整帧：1000×1300 约 14 ms、1920×1080 约 15 ms（含固定栏检测时每帧做整帧与正文两遍对齐）。对齐提速前分别约 31、30 ms，见[接缝对齐提速](seam-alignment-performance-2026-10-24.md)。
- 以上在维护者本机测得，测量时有其它编译进程占用 CPU，只作量级参考。`build.ps1` 用 `simd128` 编译，并把内容哈希写进页面的脚本地址防止缓存旧引擎。

## 验证

内置浏览器（本地静态服务、深色）逐章跑通：固定栏识别为 52/44 px 与页面一致；对比层右侧真实拼接中无底栏残留，左侧重复可数；往回滚产生视口回访而不追加；“亲手试试”滚轮驱动正常；手机宽度无横向溢出。面板隐藏时浏览器不触发 requestAnimationFrame，自测需临时用定时器代替。
