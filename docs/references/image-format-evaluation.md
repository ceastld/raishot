# JXL、相机 RAW 与 PSD 的独立评估

## 状态与共同入口

本页是 F11 的技术评估与限定实现记录，不是全格式验收清单。当前 JXL、相机 RAW 没有专用入口；PSD的有界技术样片与后继限定入口已通过各自普通回归，并进入当前Dev运行版。现有七格式小样矩阵也不包含三者。某些 RAW 容器与 TIFF 共用文件头，偶然进入 TIFF 后端不能证明已支持其 RAW 语义。

各格式须先有独立的来源识别、分配前预检和真实像素回归，再接统一文件／字节入口。来源格式必须如实披露；嵌入 JPEG 预览不能把整个 RAW 文件标成 JPEG，PSD 合并图不能称为可编辑图层文档。

新入口应延续来源实际容量、外部驻留量 R、解码私有工作区、输出、方向和颜色转换分相位计费，检查旧新缓冲共存。32MP／65535 单边及 256MiB 总额不能由新格式绕过。ICC、EXIF 和辅助元数据也须先受限，不能只限制最终 4P 输出。

既有 EXR 的实现与验证边界见[流式图片来源专题](image-animation-streams.md)，这里不重复其工作集模型。

## JXL

**最小交付范围**：静态 SDR RGB／灰度及普通透明度，先输出当前 BGRA8 查看图；同时覆盖 Modular 无损与 VarDCT 有损码流，不把只成功一种编码方式称为 JXL 支持。动画、额外深度／专色通道、任意 HDR 映射及 JPEG 重建导出独立评估。非动画码流也可能需要内部参考图，不能按“只有一个显示帧”只预留一张图。

**后端候选**：官方纯 Rust [jxl-rs 的 jxl crate](https://github.com/libjxl/jxl-rs/tree/v0.8.0/jxl)已有[v0.8.0发布](https://github.com/libjxl/jxl-rs/releases/tag/v0.8.0)，声明 BSD-3-Clause，本轮按此tag只读审计但未验证registry解析。另有纯 Rust [jxl-oxide](https://github.com/tirr-c/jxl-oxide)，声明 MIT OR Apache-2.0，提供读取、渲染和 image 适配器。候选存在或适配器可用不代替 RaiShot 验收。C++ [libjxl](https://github.com/libjxl/libjxl) 可作另一解码后端候选，其许可证为 BSD-3-Clause；静态集成可避免用户安装独立运行库，但会增加原生构建与分发审计成本。

**预算与取消成本**：需要逐项查编码输入缓存、通道平面、Modular／VarDCT 工作区、参考帧、浮点渲染和最终量化。jxl-oxide 有 [AllocTracker](https://github.com/tirr-c/jxl-oxide/blob/main/crates/jxl-grid/src/alloc_tracker.rs)，但这不等于全部分配受控：其 [辅助 box 读取器](https://github.com/tirr-c/jxl-oxide/blob/main/crates/jxl-oxide/src/aux_box.rs) 使用普通 Vec 和 Brotli 写入器，初始化也会先读取嵌入 ICC。容器辅助 box、解压元数据和 ICC 必须另有上限或安全解码视图。分块喂入可检查取消，但不能据此保证一次内部渲染计算能立即中断；正式接线前须明确这段延迟。

jxl-rs v0.8.0的[选项](https://github.com/libjxl/jxl-rs/blob/v0.8.0/jxl/src/api/options.rs)只有样本量门，不是字节额度，也没有公开分配器；[内部图像](https://github.com/libjxl/jxl-rs/blob/v0.8.0/jxl/src/image/internal.rs)直接调用标准分配。更早的[ICC解码](https://github.com/libjxl/jxl-rs/blob/v0.8.0/jxl/src/icc/mod.rs)可拥有16MiB中间流及256MiB解压配置，BasicInfo事件却在ICC完成后交付，事后4MiB检查不能保护这个阶段。[解码入口](https://github.com/libjxl/jxl-rs/blob/v0.8.0/jxl/src/api/decoder.rs)可用输入与串行runner检查取消，不请求aux可跳过其内容；但内部计算没有统一取消钩子，reset还保留图像/参考缓存，完成应drop解码器。借用caller输出缓冲也不等于内部只留最终4P。因此仅可先做已知自有小样的技术验证，任意文件入口仍缺分配前总工作集门。

**颜色与方向**：XYB／枚举颜色、原 ICC 与实际渲染结果的颜色域要分开。jxl-oxide 的[颜色接口](https://github.com/tirr-c/jxl-oxide/blob/main/crates/jxl-oxide/src/lib.rs)区分 original_icc 与 rendered_icc，宽高接口已包含方向；不可把原 CMYK 配置套给已转 RGB 的输出，也不可再次应用已处理的方向。首批只对已证明的 SDR 域授予正确的颜色状态，其余明确未管理或不支持。

**受控样本**：自有非方形色块分别编码为 Modular RGB8＋透明度、VarDCT RGB8，容器与裸码流各至少一份；已知方向、灰度和透明隐藏 RGB 做精确参考。另备 16 位／HDR、动画、额外通道、超大辅助 box／ICC、截断及超预算样本，区分合法未支持与损坏。编码工具只用于事先生成可再分发 fixture，构建和测试不调用外部程序。

**未验证**：本轮未新增依赖、下载样本或运行解码器；两个 Rust 后端的完整工作集、取消点和像素结果均未授证。下一步是锁版后的预算／颜色 API 审计，再决定是否接入。

## 相机 RAW

**最小交付范围**：先做明确标注的“相机内嵌预览查看”，不把它称为传感器显影或原精度编辑。无新原生后端的首片可限定为 DNG／受控 TIFF 型目录中的 JPEG 预览：有界解析目录，冻结所选范围，交现有严格 JPEG 入口；没有合格预览则明确不支持。传感器图像、预览尺寸和默认裁切分别记录，不能用预览尺寸伪造 RAW 原始尺寸。

**后端取舍**：全面厂商格式候选是原生 [LibRaw](https://github.com/LibRaw/LibRaw)，官方允许选择 LGPL-2.1 或 CDDL-1.0；可静态集成，不必要求用户安装额外运行库，但需要原生构建、许可证和依赖功能清单。其 [unpack_thumb／unpack_thumb_ex](https://www.libraw.org/docs/API-CXX.html) 可取默认或指定预览，JPEG 保持编码数据，其它预览可能已解成 RGB 位图。纯 Rust 候选 [rawler](https://github.com/dnglab/dnglab/tree/main/rawler) 声明 LGPL-2.1，但项目 [README](https://github.com/dnglab/dnglab/blob/main/README.md) 明确警告不适合必须承受不可信输入且不能 panic／abort 的场景，不能只因纯 Rust 就直接用于查看器任意文件入口。

**预算与多图成本**：RAW 不是单一格式。厂商压缩、CFA／X-Trans、黑电平、白平衡、相机矩阵、去马赛克、有效区和方向都影响真实显影；多拍摄帧及多个预览也不是普通分页 RGB。LibRaw 的 [max_raw_memory_mb](https://www.libraw.org/docs/API-datastruct.html) 只限制 raw buffer，不能当总工作集上限；其进度取消回调也[不覆盖所有阶段](https://www.libraw.org/docs/API-CXX.html)。元数据打开、预览提取与显影均需各自的分配前检查，未建立模型前不授予完整共享预算保证。

**颜色与资格**：相机预览已经包含相机的白平衡和色调处理，不能把它当线性传感器样本。内嵌 JPEG 与外层目录的方向应选定一次应用规则；缺 ICC 时不能根据扩展名假称已色管。初始预览应沿 Preview 操作资格，允许源文件操作，禁止把低分辨率结果冒充完整源图交编辑／OCR／导出。这里只复用操作门；相机裁切和处理后的 JPEG 未必与传感器像素一一对应，不能直接套用当前PNG采样预览的源坐标映射或区域解码模型，100%应按真实预览像素披露。

**受控样本**：先有自制小 DNG，含非方形 JPEG 预览、方向和独立传感器尺寸；再有逐文件许可明确的真实 CR2／NEF 预览样本，以及无 JPEG 预览、多个预览、坏目录／范围样本。完整显影另需真实 Bayer 与 X-Trans、12／14／16 位、不同黑电平和白平衡参考。RAW 代码的许可证不自动覆盖相机照片；需另核样本许可，本轮不下载大文件。

**未验证**：没有 RAW 后端、真实厂商像素或显影颜色验收。嵌入预览是可独立交付的第一片，不能据此关闭完整 RAW 显影目标；其成本和许可需单独决定。

## PSD

**最小交付范围**：自有小型、有界 reader，只读 PSD v1、RGB8 的保存合并图，先支持 Raw 与 PackBits。跳过有范围校验的图层区，不重建图层、混合模式、文字、智能对象或效果；不存在可用合并图时明确失败。首批拒 PSB、ZIP／ZIP prediction、CMYK／Lab／专色、16／32 位，不能静默降为错误 RGB。

**后端选择**：这一范围可按 [Adobe 官方格式规范](https://www.adobe.com/devnet-apps/photoshop/fileformatashtml/)自行实现，不新增运行库。现有纯 Rust [psd crate](https://github.com/chinedufn/psd) 声明 MIT／Apache-2.0，可作为候选审查对象，但不宜直接裸接：其 [from_bytes](https://github.com/chinedufn/psd/blob/master/src/lib.rs) 会先解析图层和资源，PSD 合并通道也复制到自有 Vec；[ZIP 分支](https://github.com/chinedufn/psd/blob/master/src/sections/image_data_section.rs)是 unimplemented!，且未支持 PSB。安全读合并图无需为全套图层解析和复制支付这份工作集。

**预算范围**：先核 26 字节头、版本／保留字段／RGB8、3 或经证明的透明通道、PSD 单边 30000 及 RaiShot 32MP 上限。各区长度、偶数字节填充、资源数量／总量、Raw 精确长度或 RLE 每行字节数都用 checked 算术和固定源范围验证；逐行还原至唯一 BGRA8 输出。资源只提取有界 ICC／必要方向信息，不保存无关 blob；来源、R、行缓冲、输出和颜色转换同账本。

**alpha 与颜色**：第四通道不能自动视为 RGBA。规范以负的图层计数指示合并结果透明度，额外 alpha／专色与此不同。透明支持须以真实合并图确认白底混合、隐藏 RGB 和直 alpha 的准确关系，不能猜。无 ICC 的 RGB 不授予 ConvertedSrgb；存在有效 RGB ICC 才沿现有颜色路径处理。查看合并图不等于保留 PSD 图层或 PSD 另存能力。

**受控样本**：自有 3×2 非方形 RGB Raw 与 PackBits 合并图，RGB 值完全一致；带图层但合并结果不同的样本证明读取的是保存合并图。另有负图层计数的透明合并图及额外 alpha 反例，含 ICC 和截断行；PSB／ZIP／CMYK／16 位必须得到明确未支持结果。fixture 与普通定向测试通过后才接统一入口，不以手工头解析成功授予像素能力。

**技术样片与限定入口**：有界 reader 及5项普通测试已通过独立只读复核，并在主工作区target以Dev完成测试构建，5项普通测试实际通过、0 ignored。仅接三通道 RGB8 Raw／PackBits、输出未色管的不透明 BGRA8；资源与行表只借用，ICC／EXIF／XMP、透明合并和其它子集明确拒绝。正图层 fixture 有完整记录及通道数据，保存合并图像素故意不同于图层；不假称验证了图层语义。

后继已将 reader 接入统一文件／字节与共享缩略入口，目录仅接纳`.psd`，来源独立标为PSD；严格邻图预取白名单不扩大。adapter不复制整幅像素，来源实际容量C、外部R、输出实际容量及64KiB控制量各计一次，短提示实际容量和固定侧车在控制预留内核对。颜色保留`SignalUnmapped`，不经普通无ICC路径授予`AssumedSrgb`；只允许查看、编辑或另存合并图的普通像素副本，提示不保留图层。两项新增入口回归覆盖身份、颜色事实和精确预算，已在后继主target Dev测试构建中实际全过、0 ignored；旧5项reader未重复。随后普通Dev三程序构建并更新，当前运行版已含限定入口；真实文件窗口、另存、色管及透明图未验证。

再后继补出真实合并图资格门：按[官方1057 Version Info字段](https://www.adobe.com/devnet-apps/photoshop/fileformatashtml/)检查借用payload的第5字节；明确否定`hasRealMergedData`时拒绝占位合并像素，前5字节截断则Invalid。缺1057仍兼容旧格式，不限制两个版本字段为1，也不解析／分配Unicode名称；重复资源中任一false都不能被true覆盖。新增一项Raw／PackBits及统一入口回归已通过后继主target Dev测试构建与实际普通运行、0 ignored；旧绿未重复。随后普通Dev三程序构建成功并更新，托盘与待命均响应，当前运行版已含此guard；真实PSD文件窗口和另存仍未验。
