# HDR 截图（2026-10-19）

## 问题

Windows 开启 HDR 后，桌面用 FP16 scRGB 合成（1.0 = 80 nits），普通 SDR 窗口会乘上“SDR 内容亮度”对应的倍数（例如 280 nits 时为 3.5）。旧版桌面复制接口 `DuplicateOutput` 把这个结果直接截断成 8 位，不除回白点，于是 HDR 屏上的截图整体过曝发白，中间调明显变亮，亮部全部压成 255。

各采集路径的情况：

| 路径 | 用途 | HDR 屏表现（修复前） |
|---|---|---|
| GDI BitBlt（`fast_capture.rs`） | 静态截图主路径 | 正常，系统给的是 SDR 映射结果 |
| DXGI `Snapshot` 首帧 | 静态截图回退、CLI | 过曝 |
| DXGI `LiveScreen` 连续刷新 | 长截图、录屏 | 过曝 |
| WGC 窗口采集（`pin_live_snapshot.rs`） | 实时窗口贴图转静态 | 普通 SDR 窗口实测正常，保留 8 位帧池；真正的 HDR 源另行验证 |

实测（HDR 屏 SDR 白点 280 nits，与 GDI 同时对照）：修复前，GDI 值 45／80／109 在 DXGI 结果里是 85／141／177，与“线性值 ×3.5 后截断再编码 sRGB”的推算一致；SDR 屏上两条路径逐像素相同。

## 做法

`capture_hdr.rs`：

- 输出经 `IDXGIOutput6::GetDesc1` 判断色彩空间为 `RGB_FULL_G2084_NONE_P2020`（HDR 开启），并通过 DisplayConfig 按 GDI 设备名查到该屏的 SDR 白点后，才改用 `DuplicateOutput1` 请求 `R16G16B16A16_FLOAT`（列表里同时给 BGRA8）。SDR 屏、查不到白点、新接口失败时都用原来的 `DuplicateOutput`，行为不变。
- 收到 FP16 帧时，用一个全屏三角形加像素着色器把源区域写进自有 BGRA8 纹理：线性值除以白点倍数，截断到 [0,1]，再按 sRGB 分段曲线编码。自有纹理仍是 BGRA8（多加渲染目标绑定），读回、旋转、录屏视频处理都不需要改。
- 首帧按 HDR 换算过的纹理，连续采集才再请求 FP16；否则两者保持同一路径。
- 着色器源码 `capture_hdr.hlsl`，预编译 DXBC 放在 `capture_hdr_shaders.rs`（页首有 fxc 重新生成命令），运行时不依赖着色器编译器。DisplayConfig 按 Win32 布局手写，不新增 windows feature。

取舍：比 SDR 白更亮的 HDR 高光（HDR 视频、游戏）直接截断为白，不做色调映射；这样普通窗口与文字的颜色能和关闭 HDR 时一致。超出 sRGB 色域的颜色同样截断。

## WGC 窗口与跨屏

窗口可同时覆盖多块显示器，但 WGC 采集项是整个 HWND，不能根据它与各屏的桌面交集就分别套用 `capture_hdr` 的白点。已有普通 SDR 窗口实测不受 HDR 屏白点放大，重复除白点会压暗；对同一窗口分段换算还可能人为制造接缝。因此生产路径继续请求 `B8G8R8A8UIntNormalized`，SDR 行为不变。

测试复用一个不激活的深色窗口，依次移到每块屏、跨共享边界（两侧各占 1/4、1/2、3/4），再移回起点；分别测试 GDI 绘制和 BGRA8 GPU 交换链。GPU 色带与 GDI 不同，移屏后交替改变，避免旧帧或 GDI 表面假通过；没有 HDR 屏或共享边界时明确打印覆盖不足。

这里的结论只覆盖普通 SDR 源。微软提醒真正的 HDR 内容可能被 BGRA8 截断，建议整条采集链使用 FP16；这不代表普通 SDR 窗口应统一除显示器白点。HDR10/scRGB 源及其参考白需要独立验证后再决定是否复用换算器。见 [WGC 官方说明](https://learn.microsoft.com/en-us/windows/apps/develop/media-authoring-processing/screen-capture) 与 [Advanced Color 白点说明](https://learn.microsoft.com/en-us/windows/win32/direct3darticles/high-dynamic-range)。

## 验证

- `rabshot-cli --all-screens`：HDR 屏与 GDI 对照，98.65% 像素完全相同、99.48% 差值在 ±1 以内；剩下的差异集中在任务栏顶边 1px 线和个别图标边缘，属于两种系统合成之间的细微差别。
- `cargo test --lib capture::`（含白点换算、字节码头校验）通过。
- 实机忽略测试 `capture::tests::live_region_refresh_matches_initial_capture_on_every_output`：每块屏先裁剪采集，清零自有纹理后按选区连续刷新，结果与首帧一致（HDR 屏走 FP16 换算）。运行：`cargo test --lib live_region_refresh -- --ignored`。
- 实机忽略测试 `pin_live_snapshot::tests::wgc_window_snapshot_matches_gdi_on_every_output`：每块屏短暂放一个不抢焦点的深色自有窗口（0–189 灰、红、绿、蓝色带），WGC 单帧（`B8G8R8A8UIntNormalized`）与 GDI、绘制值比对。HDR 屏（白点 280 nits）与 SDR 屏都是 65536/65536 像素一致，灰阶 45／81／111 原样不变，没有中间调抬高。运行：`cargo test --lib wgc_window_snapshot -- --ignored --nocapture`。
- 已增强上述专项，加入同一窗口移屏、跨屏与 GPU SDR 交换链（`pin_live_snapshot_gpu_tests.rs`）；新增测试模块尚未编译和实机验证，不能把既有 GDI 结果外推为这些场景已通过。
- 真实长截图、录屏界面流程未验；HDR 高光内容（视频、游戏）的截断效果未专门看。WGC 只验了 GDI 绘制的窗口，GPU 交换链窗口在 HDR 屏上未单独测。

## 后续

- WGC 普通 SDR 窗口在 HDR 屏的既有实测正常，保留 BGRA8；GPU 与移屏、跨屏专项结果见上方验证记录。
- 如果用户需要截 HDR 视频画面，再考虑高光色调映射（会改变接近白色的 SDR 内容，需要做成可选）。
