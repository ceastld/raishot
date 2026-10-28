# Snow Apps 媒体与工具调研

日期：2026-10-22。用途：为[媒体查看、录屏与工具整体优化计划](../references/media-viewers-recording-tools-plan-2026-10-22.md)发现遗漏行为。此处保留参考产品名与官方源码对照；公开专题只描述 RabShot 自身需求。

范围：只读查阅 [Snow Apps 官方仓库](https://github.com/mg-chao/snow-apps) 当前主分支相关文档与源码。未运行参考软件、未下载发行包、未实测画面/性能/设备。官网本次读取超时，没有用宣传补齐证据。代码可见表示存在实现路径；设计文档表示设计合同，二者都不等同实机验收。

## 调研结论

可借鉴的重点是图片查看/编辑的完整工作流，以及录屏回放/裁剪/导出的完整链路。未找到足够证据证明其独立查看器可任意打开常见视频文件，因此 RabShot 的通用视频播放属于独立需求。没有证实的 TIFF/RAW/PSD/PDF 输入、AV1/VP9 录屏编码等，公开计划均按独立技术验证安排。

参考只用于发现行为，具体实现按 RabShot 的 Windows、Rust、GPUI、文档/场景与进程边界独立设计；没有引入参考工程运行时或复制其源码。

## 行为证据与任务映射

| # | 查到的行为与证据边界 | 官方入口 | RabShot 任务 |
| --- | --- | --- | --- |
| 01 | 格式模型区分读取、写出、动画、多图和颜色；实际 codec 受构建开关影响 | [格式模型](https://github.com/mg-chao/snow-apps/blob/main/snow_image/include/snow/image/format.h)、[注册实现](https://github.com/mg-chao/snow-apps/blob/main/snow_image/src/codec_registry.cpp) | B02、F01；当前构建和机器分别探测 |
| 02 | 常见静态格式与 HEIF/AVIF/JXL/EXR/SVG 等存在注册路径；枚举不证明当前包可用 | [注册实现](https://github.com/mg-chao/snow-apps/blob/main/snow_image/src/codec_registry.cpp) | F03、F09–F11；分项样本和可选组件门 |
| 03 | 当前目录序列使用自然排序，前后图来自统一格式服务 | [目录序列](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/core/folder_sequence.cpp) | I02；1/2/10、失效文件和范围限制 |
| 04 | 查看窗连接适应窗口、原尺寸、缩放、旋转和前后图操作 | [查看窗口](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/ui/viewer_window.cpp) | I01/I03/I06；复用已有图片编辑视口 |
| 05 | 缩略图、解码、邻图预取分开调度，使用取消与请求代次 | [加载器](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/decoding/image_loader.h) | B03、I04；快速切图拒绝过期结果 |
| 06 | 解码路径应用方向，保留颜色/HDR相关信息，显示数据区分普通8位与高精度来源 | [解码实现](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/decoding/snow_image_decoder.cpp)、[图像模型](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/core/image_types.h) | F02/F07/F08；不能把 RGBA8 打开当完整颜色支持 |
| 07 | 存在条带、瓦片及工作内存预算；不同格式不一定共用相同大图路线 | [大图解码](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/decoding/snow_image_decoder.cpp) | B06、I12；先证明有界查看再扩大尺寸 |
| 08 | 动画模型带帧延时与循环次数，查看窗按计时推进；全部产品交互未实测 | [动画数据](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/core/image_types.h)、[查看窗口](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/ui/viewer_window.cpp) | F04/F05、I11；另补暂停逐帧和成品验证 |
| 09 | 动态贴图有共享截止时间、后台解码和源/全局缓存预算；这是贴图路径的证据 | [贴图动画调度](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/src/presentation/services/screenshotanimationplayback.cpp) | F04/I11；帧源可以共享，窗口生命周期独立 |
| 10 | 图片编辑配置含比例、重采样、调色板、质量、格式与透明意图 | [编辑配置](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/editing/edit_export_settings.h) | F12、T02/T03；只显示后端有效参数 |
| 11 | 图片处理将快速预览、编码产物和精确结果分状态，带代次/取消 | [编辑管线](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/editing/edit_pipeline_controller.h) | B07、F12；保存绑定最终参数产物 |
| 12 | 查看窗有复制像素/路径、定位、系统打开和回收站工作流 | [查看窗口](https://github.com/mg-chao/snow-apps/blob/main/snow_image_viewer/src/ui/viewer_window.cpp) | I10；临时图和外部文件的权限分清 |
| 13 | 录屏回放 worker 持有解码器和声音设备，命令有代次、seek与取消 | [播放实现](https://github.com/mg-chao/snow-apps/blob/main/snow-crates/crates/snow-recording-export/src/playback.rs) | V02/V04/V09/V10、A01/A02 |
| 14 | 其播放源依赖录屏 ClipSource；通用外部视频打开没有充分证据 | [播放源合同](https://github.com/mg-chao/snow-apps/blob/main/snow-crates/crates/snow-recording-export/src/playback.rs) | V03/V06/V13；独立需求，分别验外部文件 |
| 15 | 裁剪设计定义源时间、帧边界和末端预览语义 | [裁剪设计](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/docs/recording-trimming.md) | E01/E02；音画切点一致且保留源 |
| 16 | 倍速设计保留源位置与暂停状态，音频使用有界变速处理 | [裁剪与速度](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/docs/recording-trimming.md)、[播放命令](https://github.com/mg-chao/snow-apps/blob/main/snow-crates/crates/snow-recording-export/src/playback.rs) | V10、A07；暂停改速不自动播放 |
| 17 | 导出提交参数与后续界面调整分开，临时产物和重复导出有生命周期设计 | [输出设计](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/docs/recording-trimming.md) | B07、E08；任务只用提交时的参数 |
| 18 | 输出格式、视频编码、硬件策略、尺寸/FPS与预算分别建模 | [导出配置](https://github.com/mg-chao/snow-apps/blob/main/snow-crates/crates/snow-recording-export/src/config.rs) | R01/R02/R04/R07；封装和编码分开 |
| 19 | HEVC说明要求实际设备协商、适配器匹配和试编码，硬件模式不能由注册表推断 | [HEVC设计](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/docs/recording-hevc.md) | R01/R02/R05/R08；仅硬件失败不能冒充成功 |
| 20 | 实时/离线处理预算分开，质量模型关联像素、帧率与目标质量 | [质量模型](https://github.com/mg-chao/snow-apps/blob/main/snow-crates/crates/snow-recording-model/src/video_quality.rs)、[导出设计](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/docs/recording-trimming.md) | R03/R04、E04；以自产相同样本比较 |
| 21 | 运行配置有系统/麦克风/设备音源；示例处理丢包、重启和暂停恢复事件 | [录制配置](https://github.com/mg-chao/snow-apps/blob/main/snow-crates/crates/snow-recording-runtime/src/config.rs)、[双源事件](https://github.com/mg-chao/snow-apps/blob/main/snow-crates/crates/snow-audio-recorder/examples/dual_source_stream.rs) | A03–A06/A08；不把示例存在当设备全覆盖 |
| 22 | 先录后码实现保留干净源和重试渲染入口，持有使用期间的资源保护 | [延后处理](https://github.com/mg-chao/snow-apps/blob/main/snow-crates/crates/snow-recording-runtime/src/deferred.rs) | R14；收益、磁盘预算与恢复先实验 |
| 23 | 摄像头导出路径按录屏时间读取独立视频/索引/样式元数据 | [摄像头回放](https://github.com/mg-chao/snow-apps/blob/main/snow-crates/crates/snow-recording-export/src/webcam.rs) | R13；设备兼容和断开恢复仍需实机 |
| 24 | 图片分割有方向/单位/手工线；拼图 PDF 有单/多页、进度与取消输出 | [分割模型](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/include/snow_shot/presentation/imagesplitdocument.h)、[分割输出](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/src/presentation/imagesplit/imagesplitexport.cpp)、[PDF输出](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/src/presentation/collage/collagepdfexport.cpp) | T04–T06；像素完整、部分结果与取消语义独立设计 |
| 25 | 有工具命令表及构建级能力隔离；未证实与本次所需人工验收开放流程相同 | [工具命令](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/include/snow_shot/presentation/screenshotcanvastools.inc)、[能力开关](https://github.com/mg-chao/snow-apps/blob/main/snow_shot/include/snow_shot/app/edition.h) | B04/T01；内部/实验/正式状态由 RabShot 自己定义 |

## RabShot 本次独立判断

- 原有录屏结果 Player 已可解码视频，但声音依赖辅助 WAV；主界面录屏仍交系统播放器。先补内嵌音轨与日常入口，V01 继续检查用户实际故障。
- 现有图片解码依赖的格式范围不能直接当产品支持列表；F01 用当前构建实际样本建立基线，颜色、动画、多页和写出分别算能力。
- 现有旋转/镜像/灰度/反色/亮度、标注工具、保存事务和主界面 M0–M2继续复用。富文本 S1和长图原型已有专人/专题，不重复立项。
- 特殊格式、高级编码、摄像头与可选模型先内部开发；自动验证和人工验收分别记录，正式入口按单项结论开放。

本记录只支持需求与方案取舍，不对两个程序的实际兼容性、运行速度、画质或稳定性作比较结论。
