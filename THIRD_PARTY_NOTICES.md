# 第三方组件与许可证

RaiShot 自有代码使用 [MIT 开源许可证](LICENSE)，允许商用、修改和再分发，分发时保留适用的版权与许可声明。以下是当前锁定源码与运行资产的通知入口；第三方许可证不因 RaiShot 的许可证而改变，不能将第三方组件统一重标为 MIT。本文件不替代分发包中各组件的完整许可证和版权通知。

分发源码或二进制时，保留项目许可及适用的第三方许可、版权与通知。此前已按 GPL 合法授出的版本，其既有许可权利与分发义务不因本次调整而改变；不能用新许可覆盖它们。项目许可范围见[许可说明](docs/license.md)。

## 主要 Rust 组件

依据 `Cargo.lock` 与对应已下载 crate 的 `Cargo.toml` 核对声明，包含构建、开发和其他平台的锁定依赖，不能视为实际 Windows 发布包的链接清单。

| 组件 | 锁定版本 | 声明许可证 | 来源与说明 |
| --- | --- | --- | --- |
| GPUI | 0.2.2 | Apache-2.0 | [上游 crate](https://docs.rs/crate/gpui/0.2.2)；本地源码与完整许可在 `vendor/gpui`，补丁说明见 [vendor/README.md](vendor/README.md) |
| omnitrigger-core / omnitrigger-windows | 0.1.0 | MIT | 本地固定源码在 `vendor/omnitrigger`；Copyright (c) 2026 OmniTrigger contributors；完整许可见 [vendor/omnitrigger/LICENSE](vendor/omnitrigger/LICENSE) |
| ort / ort-sys | 2.0.0-rc.13 | MIT OR Apache-2.0 | [上游](https://github.com/pykeio/ort/tree/v2.0.0-rc.13)；Rust 绑定，与原生 ONNX Runtime 分开许可 |
| quircs | 0.10.3 | MIT | [上游](https://github.com/dignifiedquire/quircs)；RaiShot 使用 Rust 移植实现，保留其 quirc 来源通知 |
| image | 0.25.10 | MIT OR Apache-2.0 | [上游](https://github.com/image-rs/image) |
| gif | 0.14.2 | MIT OR Apache-2.0 | [上游](https://github.com/image-rs/image-gif) |
| resvg / usvg | 0.45.1 | Apache-2.0 OR MIT | [上游](https://github.com/linebender/resvg/tree/v0.45.1) |
| RaTeX（parser / layout / types / render / unicode-font 及配套字体模块） | 0.1.15 | MIT；内嵌 KaTeX 字体为 SIL-OFL-1.1 | [上游](https://github.com/erweixin/RaTeX)；公式在本机排版，完整许可见本文末尾 |
| pulldown-cmark | 0.13.0 | MIT | [上游及完整许可证](https://github.com/pulldown-cmark/pulldown-cmark/blob/v0.13.0/LICENSE)；Copyright 2015 Google Inc. All rights reserved.；本机解析 Markdown |
| tiny-skia | 0.11.4 | BSD-3-Clause | [上游](https://github.com/linebender/tiny-skia) |
| rusqlite / libsqlite3-sys | 0.40.2 / 0.38.2 | MIT | [上游](https://github.com/rusqlite/rusqlite)；启用 bundled，静态链接随包 SQLite 3.53.2，无单独数据库服务或 DLL |

SQLite 核心源码声明放弃版权，许可说明见[官方说明](https://sqlite.org/copyright.html)。rusqlite 与 libsqlite3-sys 包装层保留其 MIT 许可：Copyright (c) 2014 The rusqlite developers。以下文本随这两个组件的分发保留：

```text
Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL
THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
IN THE SOFTWARE.
```

其余锁定依赖主要声明 MIT、Apache-2.0、BSD、ISC、Zlib、Unicode-3.0 或多许可证任选其一。清单中未发现 `GPL-2.0-only` 或未声明许可证的包；这只是 manifest 检查，尚未逐件审核源码内的许可与通知。需要单独保留或确认的声明包括：

- `encoding_rs 0.8.42`：`(Apache-2.0 OR MIT) AND BSD-3-Clause`；`ring 0.17.14`：`Apache-2.0 AND ISC`；`libfuzzer-sys 0.4.13`：`(MIT OR Apache-2.0) AND NCSA`。`AND` 表示同时保留相关许可，不可当作任选一个。
- `cbindgen 0.28.0`：MPL-2.0。发布前确认它在所选构建中的用途，若分发其源码或二进制，按该组件的许可准备材料。
- `webpki-roots 0.26.11 / 1.0.9`：CDLA-Permissive-2.0；Unicode 数据组件：Unicode-3.0；部分 Rust 系统绑定：Apache-2.0 WITH LLVM-exception。
- `r-efi 5.3.0 / 6.0.0` 含 LGPL-2.1-or-later 选项，同时提供 MIT 或 Apache-2.0；发布时记录实际选用的许可。
- 部分历史 crate 使用斜线表达双许可证；本轮原样读取声明，没有将其自动解释为标准 SPDX 表达式。

尚未生成实际发布包的完整第三方版权清单，未据此宣称分发合规检查已经完成。

## 随源码固定的组件许可

GPUI 的完整 Apache-2.0 许可及 Copyright 2022 - 2025 Zed Industries, Inc. 保存在 [vendor/gpui/LICENSE-APACHE](vendor/gpui/LICENSE-APACHE)；OmniTrigger 的完整 MIT 许可及上述版权通知保存在 [vendor/omnitrigger/LICENSE](vendor/omnitrigger/LICENSE)。这些组件继续按各自的许可证分发。

`Tools/package.ps1 -IncludeProjectNotices` 将两份原文分别放入便携包根目录的 `LICENSE-GPUI-APACHE.txt` 与 `LICENSE-OmniTrigger.txt`，同时保留项目 `LICENSE` 和本通知文件；源码分发保留 `vendor/` 中的原文件。此项补齐固定源码组件的材料，不代替其余依赖的分发通知。

## WebP 原生后端：libwebp-sys / libwebp

本节按已发布的 `libwebp-sys 0.14.4` 源包核对。Rust 包装层的 [Cargo.toml](https://github.com/NoXF/libwebp-sys/blob/3a40281fd97537b693174acb7375d64d3b471a56/Cargo.toml) 声明 MIT；未启用 `system-dylib` 时，[build.rs](https://github.com/NoXF/libwebp-sys/blob/3a40281fd97537b693174acb7375d64d3b471a56/build.rs) 通过 `cc` 将随包的 libwebp 与 SharpYUV C 源码编译为静态库 `webpsys`。关闭默认 features 不会改用系统动态库，也不会免除原生源码的通知要求。

源包对应的上游提交将 `vendor` 固定为 [libwebp 源码](https://github.com/webmproject/libwebp/tree/4fa21912338357f89e4fd51cf2368325b59e9bd9)，其 [configure.ac](https://github.com/webmproject/libwebp/blob/4fa21912338357f89e4fd51cf2368325b59e9bd9/configure.ac) 声明版本 1.6.0；不能用包装层版本或随时变化的 master 代替该原生版本。libwebp 使用 BSD-3-Clause，并附有额外专利授权。分发随包 C 源码或链接它的二进制时，应保留源码中的适用版权头，并随分发材料提供完整 `COPYING` 与 `PATENTS`；包装层的 MIT 声明不能替代这两份材料。以下原文取自已发布源包的 `vendor/COPYING` 和 `vendor/PATENTS`。

包装层通知材料仍有来源缺口：该 0.14.4 源包根目录没有独立的 MIT 许可证文件，README 与 Rust 绑定源码中也未附完整 MIT 许可及对应版权行。manifest 的 authors 字段不等同于版权通知；本文件不据此推定版权人或年份。分发包装层时仍需从上游取得并保留其完整 MIT 许可与适用版权通知，不能将下面 Google 的 libwebp 版权行套用于 Rust 包装层。

### libwebp COPYING

来源：[固定原生源码的 COPYING](https://github.com/webmproject/libwebp/blob/4fa21912338357f89e4fd51cf2368325b59e9bd9/COPYING)。

```text
Copyright (c) 2010, Google Inc. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

  * Redistributions of source code must retain the above copyright
    notice, this list of conditions and the following disclaimer.

  * Redistributions in binary form must reproduce the above copyright
    notice, this list of conditions and the following disclaimer in
    the documentation and/or other materials provided with the
    distribution.

  * Neither the name of Google nor the names of its contributors may
    be used to endorse or promote products derived from this software
    without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

### libwebp PATENTS

来源：[固定原生源码的 PATENTS](https://github.com/webmproject/libwebp/blob/4fa21912338357f89e4fd51cf2368325b59e9bd9/PATENTS)。

```text
Additional IP Rights Grant (Patents)
------------------------------------

"These implementations" means the copyrightable works that implement the WebM
codecs distributed by Google as part of the WebM Project.

Google hereby grants to you a perpetual, worldwide, non-exclusive, no-charge,
royalty-free, irrevocable (except as stated in this section) patent license to
make, have made, use, offer to sell, sell, import, transfer, and otherwise
run, modify and propagate the contents of these implementations of WebM, where
such license applies only to those patent claims, both currently owned by
Google and acquired in the future, licensable by Google that are necessarily
infringed by these implementations of WebM. This grant does not include claims
that would be infringed only as a consequence of further modification of these
implementations. If you or your agent or exclusive licensee institute or order
or agree to the institution of patent litigation or any other patent
enforcement activity against any entity (including a cross-claim or
counterclaim in a lawsuit) alleging that any of these implementations of WebM
or any code incorporated within any of these implementations of WebM
constitute direct or contributory patent infringement, or inducement of
patent infringement, then any patent rights granted to you under this License
for these implementations of WebM shall terminate as of the date such
litigation is filed.
```

## OCR 与原生运行库

- ONNX Runtime 1.24.4 使用 [MIT](https://github.com/microsoft/onnxruntime/blob/v1.24.4/LICENSE)。`dependencies/ppocr-cpu.json` 与 `ppocr-small-cpu.json` 固定官方压缩包来源、大小和 SHA-256，并包含 `LICENSE-ONNXRuntime.txt`、`THIRD-PARTY-ONNXRuntime.txt` 的身份。分发时保留这两份文件。
- PP-OCRv6 Tiny / Small 检测、识别模型与 PP-LCNet 方向模型来自清单中固定提交的 PaddlePaddle 官方 ONNX 仓库。已分别核对下列固定模型卡，其许可证元数据均明确声明 Apache-2.0：
  - Tiny：[检测模型](https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_det_onnx/blob/2ba1506c0380b8f0b03dd142459aac66d4421f6c/README.md)、[识别模型](https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_rec_onnx/blob/2612ab37152ae0a677521bae4e1e3d4fb4cf7c30/README.md)。
  - Small：[检测模型](https://huggingface.co/PaddlePaddle/PP-OCRv6_small_det_onnx/blob/28fe5895c24fd108c19eb3e8479f4ab385fbfc62/README.md)、[识别模型](https://huggingface.co/PaddlePaddle/PP-OCRv6_small_rec_onnx/blob/b8f84f0b80c529de40b4fbb3544b84fa7233a513/README.md)。
  - 共用的 [PP-LCNet 方向模型](https://huggingface.co/PaddlePaddle/PP-LCNet_x0_25_textline_ori_onnx/blob/aea1f18d97338aaf84b463f90192f2820524a00a/README.md)。
- OCR 字典与 `LICENSE-PaddleOCR.txt` 来自 [固定 PaddleOCR 源码](https://github.com/PaddlePaddle/PaddleOCR/blob/b03f46425e8ff4442b268ce449e3eef758146cd4/LICENSE)，使用 Apache-2.0。分发模型与字典时保留该许可及适用的上游版权通知。
- 可选表格结构模型由 `dependencies/ppocr-table-cpu.json` 固定为 RapidAI/RapidTable v2.0.0 的 `slanet-plus.onnx`，通过 [RapidAI 的 ModelScope 模型仓库](https://www.modelscope.cn/models/RapidAI/RapidTable) 下载。PaddlePaddle 的 [SLANet_plus 模型卡](https://huggingface.co/PaddlePaddle/SLANet_plus/blob/bae6e5f8c3c4e7da0c0b7639fdf3228fe76184e2/README.md) 声明 Apache-2.0；RapidAI 模型仓库声明 Apache License 2.0，[RapidTable v2.0.0](https://github.com/RapidAI/RapidTable/blob/v2.0.0/README.md) 说明其 ONNX 转换来源，并提供 [Apache-2.0 许可证](https://github.com/RapidAI/RapidTable/blob/v2.0.0/LICENSE)。分发表格模型时需同时保留转换方适用的许可与通知，不能仅以 PaddleOCR 的许可文件代替。
- 当前 CPU 档不分发 DirectML。若以后启用，应单独保留 DirectML 的许可和通知，不能归入 ONNX Runtime 的 MIT 许可。
- Windows 系统字体、Media Foundation 与系统 API 使用目标电脑的系统组件。本仓库 `assets/` 未发现字体文件，不把系统字体复制到分发包。
- VC++ 可再发行运行库采用 Microsoft 的分发条款，需按实际使用的 Visual Studio 版本保留材料；它不属于 GPL 或 MIT 的项目代码。

## 图标与其他资源

RaiShot 的 SVG 与 ICO 位于 `assets/`，设计与生成方式见 [assets/icons/README.md](assets/icons/README.md)；`app.ico` 由仓库中的 `app.svg` 经 `Tools/render-tray-icon.py` 生成。未另行声明的自有资源适用项目许可；若引入第三方资源，应保留其来源与独立许可，不能将其改为项目许可。

`assets/color/srgb.icc` 的内嵌描述为 lcms sRGB built-in，版权字段声明 `No copyright, use freely`，保留其原文件与声明。

公开源码不包含 `vendor/gpui/examples/` 的上游示例及示例图片、GIF 等资源；GPUI 的源码版权头与 `vendor/gpui/LICENSE-APACHE` 保留。

## RaTeX 与内嵌 KaTeX 数学字体

```text
MIT License

Copyright (c) erweixin

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

---

Third-party fonts bundled in this repository (KaTeX_* .ttf / .woff / .woff2)
are not under the MIT License above. They are licensed under the SIL Open Font
License, Version 1.1. See THIRD_PARTY_NOTICES.txt and the licenses/ directory.

KaTeX font files (KaTeX_*.ttf)
================================

These TrueType fonts are taken from the KaTeX project:
  https://github.com/KaTeX/KaTeX

They are licensed under the SIL Open Font License, Version 1.1. The full
license text is in OFL.txt in this directory.

KaTeX's own MIT license applies to its software, not to the font binaries;
font licensing follows OFL as described in KaTeX's documentation and
upstream repository.

SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007
-----------------------------------------------------------

PREAMBLE
The goals of the Open Font License (OFL) are to stimulate worldwide
development of collaborative font projects, to support the font creation
efforts of academic and linguistic communities, and to provide a free and
open framework in which fonts may be shared and improved in partnership
with others.

The OFL allows the licensed fonts to be used, studied, modified and
redistributed freely as long as they are not sold by themselves. The
fonts, including any derivative works, can be bundled, embedded,
redistributed and/or sold with any software provided that any reserved
names are not used by derivative works. The fonts and derivatives,
however, cannot be released under any other type of license. The
requirement for fonts to remain under this license does not apply
to any document created using the fonts or their derivatives.

DEFINITIONS
"Font Software" refers to the set of files released by the Copyright
Holder(s) under this license and clearly marked as such. This may
include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the
copyright statement(s).

"Original Version" refers to the collection of Font Software components as
distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to a
new environment.

"Author" refers to any designer, engineer, programmer, technical
writer or other person who contributed to the Font Software.

PERMISSION & CONDITIONS
Permission is hereby granted, free of charge, to any person obtaining
a copy of the Font Software, to use, study, copy, merge, embed, modify,
redistribute, and sell modified and unmodified copies of the Font
Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components,
in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled,
redistributed and/or sold with any software, provided that each copy
contains the above copyright notice and this license. These can be
included either as stand-alone text files, human-readable headers or
in the appropriate machine-readable metadata fields within text or
binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font
Name(s) unless explicit written permission is granted by the corresponding
Copyright Holder. This restriction only applies to the primary font name as
presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font
Software shall not be used to promote, endorse or advertise any
Modified Version, except to acknowledge the contribution(s) of the
Copyright Holder(s) and the Author(s) or with their explicit written
permission.

5) The Font Software, modified or unmodified, in part or in whole,
must be distributed entirely under this license, and must not be
distributed under any other license. The requirement for fonts to
remain under this license does not apply to any document created
using the Font Software.

TERMINATION
This license becomes null and void if any of the above conditions are
not met.

DISCLAIMER
THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM
OTHER DEALINGS IN THE FONT SOFTWARE.

```

## pulldown-cmark

以下为 `pulldown-cmark 0.13.0` 源包的完整 [MIT 许可证](https://github.com/pulldown-cmark/pulldown-cmark/blob/v0.13.0/LICENSE)：

```text
The MIT License

Copyright 2015 Google Inc. All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```
