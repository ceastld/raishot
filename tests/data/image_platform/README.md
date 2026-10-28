# 系统 HEIF / AVIF 解码真实样本

这些样本保存自官方实现的公开原始文件，均小于 64 KiB。没有重新编码或拼接 `ftyp`，也不需要在测试时下载。

| 文件 | 字节数 | 官方可证事实与用途 | 许可 |
| --- | ---: | --- | --- |
| [white_1x1.avif](white_1x1.avif) | 305 | libavif 编码的单白像素；1×1、单图、AV1 profile 1、8 位 YUV 4:4:4。用于真实 AVIF 静态解码。 | BSD-2-Clause |
| [kodim03_yuv420_8bpc.avif](kodim03_yuv420_8bpc.avif) | 25430 | 常规尺寸静态基线；768×512、AV1 profile 0、8位4:2:0，与1×1白色样本分别记录能力。 | Kodak unrestricted usage |
| [colors-animated-8bpc.avif](colors-animated-8bpc.avif) | 1235 | 150×150、8 位 YUV 4:2:0、无 alpha、5 帧。用于检查系统实际暴露的帧数与选帧边界；不代表所有系统都能暴露全部帧。 | BSD-2-Clause |
| [hevc-confwin-420-8bit-left.heic](hevc-confwin-420-8bit-left.heic) | 4545 | HEVC Main、8 位 YUV 4:2:0，编码画布 256×128，经 SPS 左侧裁剪 6 个亮度像素输出 250×128。单图，无 `clap`、`irot` 或 `imir`。 | MIT |
| [hevc-confwin-420-10bit-all.heic](hevc-confwin-420-10bit-all.heic) | 2597 | HEVC Main 10、10 位 YUV 4:2:0，编码画布 256×128，SPS 裁剪后 248×122，左上起点为 (6, 4)。单个 intra 图项，CICP 为 sRGB 传递的 `1/13/6` full range，不是 PQ／HLG HDR。 | MIT |
| [colors_hdr_rec2020.avif](colors_hdr_rec2020.avif) | 24065 | 主图与 HDR gain map 均为 200×200、10 位 YUV 4:4:4。用于实际高位深码流头与受控容器限制回归；原容器当前不支持。 | BSD-2-Clause |

来源及许可依据：

- AVIF 原文件：[white_1x1.avif](https://raw.githubusercontent.com/AOMediaCodec/libavif/main/tests/data/white_1x1.avif)、[colors-animated-8bpc.avif](https://raw.githubusercontent.com/AOMediaCodec/libavif/main/tests/data/colors-animated-8bpc.avif)。[逐文件说明](https://github.com/AOMediaCodec/libavif/blob/main/tests/data/README.md) 明确采用与 libavif 相同的许可；适用的 BSD 两条款正文保存在 [LICENSE-libavif.txt](LICENSE-libavif.txt)，取自[官方 LICENSE 的基础许可段](https://github.com/AOMediaCodec/libavif/blob/main/LICENSE)。其余条目针对其他源码，不随测试图像引入。
- 常规尺寸 AVIF [原文件](https://raw.githubusercontent.com/AOMediaCodec/libavif/main/tests/data/io/kodim03_yuv420_8bpc.avif)在官方[逐文件说明](https://github.com/AOMediaCodec/libavif/blob/main/tests/data/README.md#files-kodim)标为Kodak unrestricted usage；原始字节完整保存。BMFF有且仅有一个`av01`主图项，没有`moov`或`iref`，`pixi`为3×8，`nclx`为`1/13/6` full range；专用预检另将声明绑定到实际AV1头，不能仅按文件名授予像素能力。同名Y4M是来源素材，不是此有损AVIF的逐像素解码oracle；原始／冻结输出相等也不是独立RGB金值。
- HEIC [原文件](https://raw.githubusercontent.com/libheif-org/heif-testcorpus/master/corpus/hevc-conformance-window/hevc-confwin-420-8bit-left.heic)来自 libheif 官方测试语料。[该目录说明](https://github.com/libheif-org/heif-testcorpus/blob/master/corpus/hevc-conformance-window/README.md) 明确图像由项目自行生成、没有第三方素材，图像采用 MIT；[官方许可](https://github.com/libheif-org/heif-testcorpus/blob/master/LICENSE)保存在 [LICENSE-heif-testcorpus.txt](LICENSE-heif-testcorpus.txt)。
- Main 10 HEIC [原文件](https://raw.githubusercontent.com/libheif-org/heif-testcorpus/master/corpus/hevc-conformance-window/hevc-confwin-420-10bit-all.heic)来自同一目录，沿用上述 MIT 许可。[目录表及颜色说明](https://github.com/libheif-org/heif-testcorpus/blob/master/corpus/hevc-conformance-window/README.md) 明确其 10 位 4:2:0、编码与裁剪尺寸，以及 VUI 和 `nclx` 都为 `1/13/6` full range。此样本保持原始字节；纯头回归不能证明 P010 原生输出、10 位像素精度或 HDR 映射已验证。
- 高位深 AVIF [原文件](https://raw.githubusercontent.com/AOMediaCodec/libavif/main/tests/data/colors_hdr_rec2020.avif)属于[官方逐文件说明](https://github.com/AOMediaCodec/libavif/blob/main/tests/data/README.md)的 `colors*_hdr_*.avif` 组，明确采用与 libavif 相同的许可，并说明图案为项目自行创建。沿用本目录 [BSD 两条款许可](LICENSE-libavif.txt)。文件保持原始字节，未删除辅助图项、EXIF 或 XMP。

只读检查前三个文件的 BMFF 元数据，分别确认 `ftyp` 主品牌为 `avif`、`avis`、`heic`，`ispe` 尺寸与上表相符；动画 `stsz` 声明 5 个样本。前三个文件各有一个静态图像项、三通道 `pixi`，没有 alpha 辅助项。上述检查没有调用 codec。

## 高位深 HDR 样本的实际头

`colors_hdr_rec2020.avif` 的 `ftyp` 主品牌为 `avif`，兼容品牌为 `avif/mif1/miaf/MA1A`，没有 `moov` 序列轨道。只读检查原始 BMFF 目录及两个图项各自的实际 AV1 sequence/frame 头，得到：

| 图项 | 编码与输出尺寸 | 原始位深与采样 | CICP 与范围 | 实际 sequence/frame 头 |
| --- | --- | --- | --- | --- |
| 主图 item 1 | sequence 最大画布、`ispe` 与帧显示尺寸均为 200×200 | `pixi` 与 sequence 均为 10 位、三通道 YUV 4:4:4 | 容器 `nclx` 与 sequence 均为 `9/16/9`，full range；BT.2020/PQ/BT.2020 non-constant luminance | profile 1，`still_picture=1`、`reduced_still_picture_header=1`、`enable_superres=0`、`film_grain_params_present=0`、`render_and_frame_size_different=0` |
| 辅助 item 4 | sequence 最大画布、`ispe` 与帧显示尺寸均为 200×200 | `pixi` 与 sequence 均为 10 位、三通道 YUV 4:4:4 | 容器 `nclx` 与 sequence 均为 `1/13/6`，full range | 同为 profile 1、单 still/reduced 头、superres 与 film grain 关闭、显示尺寸不另改 |

两个编码图项各有一个 sequence OBU 和一个 frame OBU，没有 OBU layer extension。`auxl` 将 item 4 关联至 item 1，`auxC` 的完整类型为 `urn:com:photo:aux:hdrgainmap`；这是 HDR gain map 辅助项，没有 alpha 类型辅助项。主图 `clli` 声明 MaxCLL=470、MaxFALL=263；这些只是头部事实。

当前受控静态来源拒绝此辅助关系，完整原文件应保持明确的 `Unsupported`。可单独用主图的原始配置和编码 extent 做纯头正向回归；不能把该结果称为原容器可解码。未调用像素解码器，也未核验 10 位像素精度、8 位显示降位、gain map 应用、PQ tone mapping 或最终色彩。不能从 CICP 标签或纯头预检通过推断 HDR 映射正确。

## 条件原生测试建议

- 默认测试包含纯格式识别、目录与码流头预检、输入/输出预算及预取消。真实系统解码使用独立的 Windows 原生测试，默认忽略并明确由维护者选择运行；不安装组件或联网取样。
- 静态解码应验证 AVIF 输出 1×1、HEIC 输出 250×128，BGRA 长度分别为 4 和 128000，alpha 全为 255。HEIC 的 256×128 必须由实际 SPS 检查确认，不能单据说明文件或 `ispe` 代填；与当前系统帧同显示尺寸的候选事实不一致时仍显示未知。这些样本不提供非平凡方向归一的验收依据。
- 官方将白像素称为默认质量编码，未提供解码后精确 RGB 金值；本目录不虚构逐字节 BGRA 期望。HEIC 有[官方重建 YUV 参考](https://github.com/libheif-org/heif-testcorpus/blob/master/corpus/hevc-conformance-window/hevc-confwin-420-8bit-left.yuv420p.raw)，未复制到本目录；转成 BGRA 后还涉及色彩与上采样策略，不能直接当 BGRA 金值。
- 动画先验证首帧的实际像素尺寸、长度和不透明性，再检查系统返回的帧数。系统若只公开一帧，应明确记录能力范围；不能将此结果写成已支持 5 帧动画或已确认主图。选帧测试只能使用系统公开的合法索引，并验证越界会明确失败。
- 只有 `ComponentUnavailable` 可以作为缺少系统组件的条件跳过。已成功创建解码器后，真实样本的 `Invalid`、`Native` 等失败不能伪装成解码成功。未运行原生验证时，结果保持未验。

源码已加入真实样本的目录及码流头用例：白色AVIF唯一序列头／帧，HEIC裁前尺寸／crop／位深／DPB与直接空RPS，以及错误关联、非主图坏头和跨extent范围。实际回归与原生运行范围统一见[图片专题](../../../references/image-animation-streams.md)，不将头部事实视为像素精度或HDR映射通过。
