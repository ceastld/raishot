# 录屏切换延迟与预览清晰度（2026-10-13）

## 预览发虚

- **主因：预览拉伸。** 预览用 `img()` 按选区逻辑尺寸布局；Taffy 开启取整，125% 下 1012×876 的视频被画成 1013×877（`paint_image` 的 Snap 模式再对尺寸 `ceil`），整帧双线性重采样，文字整体发软。
  改为 `canvas` + `paint_image_nearest_exact`，按解码帧物理尺寸 1:1 绘制（与贴图的 `paint_image_exact` 同理）。奇数选区编码时去掉 1px，预览右／下缘留 1px 黑边而不再拉伸。
- **次因：编码码率。** 用户当前预设“压缩 MP4”（0.07 bit/像素）。实测成片 1012×876 约 2 Mbps；编码器为 NVIDIA H.264 MFT，默认 CBR，`QualityVsSpeed=0` 和低延迟模式对 NVENC 实测无影响。
  对照基线为 x264 CRF 23、关 AQ／mb-tree。同一段 1012×876 文字滚动素材经 ffmpeg `h264_mf`（同一 MFT）对比：CBR 静止 48.3 dB／滚动 51.3 dB；PeakConstrained/Unconstrained VBR 48.8／54.5 dB，体积 +19%；高清档 CBR 53.5／56.4 dB。
  静止画面差距小，滚动时差 3 dB。尚未改码率控制（通过 ICodecAPI 在 SinkWriter 设好类型后再设置，NVENC 是否接受未验证）。若仍觉得糊，先切“高清 MP4”。

## 截图 → 录屏切换

Debug 版 `RUSTSHOT_TRACE=1 kirie-gpui --recording-ui-smoke`（热启动）：GPUI 窗口约 120ms；边框约 320ms，其中 8 个缩放柄窗口约 220ms（本机 `CreateWindowExW` 单个 6–27ms，受其它软件钩子影响）；首帧约 440ms。点开始到首帧采集约 90ms，不是瓶颈。

改动：

- 截图层不再先隐藏：先打开录屏宿主再隐藏冻结截图层，切换期间不露出空白桌面（`capture_view.rs::record`）。
- 录屏边框 `LongCaptureBorder::new_recording` 只建四条边，首帧呈现后 `on_next_frame` 再 `create_resize_handles`；长截图仍走 `new()` 一次建全。
- 录屏缩放柄改为不可见的整边／四角拖动区（alpha=1 保留命中，alpha=0 会穿透），即窗口式边框缩放，不画点。

`RECVIEW window/border/first render/resize handles` 与 `REC ...` 两组 trace 保留，可直接复测。

## 结果窗口拖动

`recording_preview.rs`：按下 `SetCapture` 记录光标，超过 3px（按 DPI）才算拖动，`place_later` 移动预览窗；未拖动的松开切换播放。新左上角经 `MoveHandler` 交给 `RecordingView::move_result`，平移选区、`border.update_region`、工具栏锚点并 `relayout`。重录沿用新位置。

## 编解码实验（2026-10-14）

`src/recording_quality_lab.rs`（默认忽略的测试）：同一段无损 1012×876 素材（60 帧静止 + 90 帧每帧滚 5px），走产品 GPU 编码路径（`Mp4Writer::new_tuned_for_lab`，BGRA→NV12→NVENC）和 x264 基线（CRF 23／18，关 AQ／mb-tree），全部用预览解码路径解码后逐帧比 RGB／亮度 PSNR，导出裁图、误差图、MP4 到 `target/qa/codec-lab/`。测试台 `#/codec` 页做 1:1 物理像素滑动／并排／闪切／误差对比和播放。

```
RUSTSHOT_LAB_SOURCE=target/qa/codec-lab-shot.png cargo test --lib recording_quality_lab -- --ignored --nocapture
```

本机 RTX 4060（NVIDIA H.264 Encoder MFT）结果（亮度 PSNR 静止／滚动，码率 kbps）：

| 方案 | 亮度 dB | RGB 静止 | kbps |
|---|---|---|---|
| 压缩 CBR（当前默认） | 44.5／47.9 | 40.0（首帧 36.8） | 1004 |
| 高清 CBR | 50.8／54.7 | 42.0 | 1646 |
| 压缩 + 峰值 VBR | 46.6／51.6 | 40.9 | 1234 |
| VBR 0.12 bit/像素 | 50.8／55.3 | 42.1 | 1795 |
| 高清 + 峰值 VBR | 51.8／55.8 | 42.3 | 1986 |
| x264 CRF 23（标准基线） | 50.5／52.5 | 42.0 | 459 |
| x264 CRF 18（高清基线） | 53.7／55.9 | 42.5 | 623 |
| 无损 4:2:0（ffmpeg 解码） | 58.5／62.8 | 45.2 | — |

- 主因是编码：压缩档 CBR 首个 I 帧比特不足，静止内容之后基本不再补细节。只有码率控制模式（峰值 VBR）和码率有效；`AVLowLatencyMode`、`AVEncCommonBufferSize`、`QualityVsSpeed` 对 NVENC 无效（输出逐字节相同），Quality 模式更差。
- 次因是预览解码：同一高清文件 RGB 软件 XVP 42.0、D3D11 硬解 42.9、ffmpeg 精确插值 43.8 dB，亮度相同，差在彩色边缘。
- 4:2:0 本身把 RGB 上限压在约 45 dB，所有 4:2:0 编码都受此限制。
- 待定（等用户看对比）：两档改峰值 VBR（峰值 2 倍），压缩档提到约 0.12 bit/像素；预览改 D3D11 解码路径需先测性能。
