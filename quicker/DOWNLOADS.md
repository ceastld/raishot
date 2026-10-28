# 程序与识别资源从哪里下载

Quicker 动作的程序包由 Quicker 依赖服务分发，不从 GitHub Release 下载。普通截图首次只需程序包；文字识别、表格识别的模型在首次使用相应功能时准备，之后复用本机缓存。

| 资源 | 下载时机与大小 | 实际来源 |
| --- | --- | --- |
| Windows 程序包 | 无托盘时启动或手动检查依赖，所需版本缓存缺失才下载；0.4.13 样本约 20.45 MB | Quicker 查询 `cea.rustshot` 的依赖版本，文件来自 `files.getquicker.net/_packages/cea.rustshot/…` |
| 默认 Tiny 文字识别 | 首次完整识别，合计约 7.30 MB；仅查找文字区域约 1.79 MB | `s3.bitiful.net/quicker-pkgs/rustshot/dependencies/…`，另有 PaddlePaddle 的 Hugging Face 官方模型源与 GitHub Raw 字典／许可备用 |
| Small 文字识别 | 显式选择后下载；全新缓存约 32.15 MB，已有 Tiny 后新增约 31.11 MB | Bitiful 镜像及官方备用源 |
| 表格结构模型 | 首次表格识别，额外约 7.77 MB；完整表格识别还需要所选档位的文字识别模型 | 当前只有 `www.modelscope.cn/models/RapidAI/RapidTable/resolve/v2.0.0/slanet-plus.onnx`，没有备用镜像 |
| ONNX Runtime | 正式包已包含，正常使用无需另下 | 包内文件缺失或不匹配时，才回退至 Bitiful 小包；官方 GitHub 完整包作为备用 |

MB 使用十进制计算。固定版本、文件大小与 SHA256 以 `dependencies/ppocr-cpu.json`、`ppocr-small-cpu.json` 和 `ppocr-table-cpu.json` 为准。模型清单中的缓存标识未变化时，软件升级后继续复用；不随每次截图重新下载。

本机 V1 的程序缓存位于系统“文档”目录下的 `Quicker/_packages/cea.rustshot/<版本>/`；系统文档位置可以被用户重定向。RaiShot 的识别资源位于 `%LOCALAPPDATA%/RaiShot/ocr/`；旧版 RabShot 目录由品牌迁移逻辑处理。

## 下载托管与模型源头

Bitiful 是 RaiShot 维护的下载镜像，模型原始发布者分别记录在清单的 `upstream_url` 中。下载镜像与模型开发者是不同角色。

| 其它资源 | 原始发布者与核对入口 |
| --- | --- |
| Tiny 检测／识别模型 | PaddlePaddle 官方 Hugging Face 的 [检测固定版本](https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_det_onnx/tree/2ba1506c0380b8f0b03dd142459aac66d4421f6c)与[识别固定版本](https://huggingface.co/PaddlePaddle/PP-OCRv6_tiny_rec_onnx/tree/2612ab37152ae0a677521bae4e1e3d4fb4cf7c30) |
| Small 检测／识别模型 | 同一官方组织的[检测固定版本](https://huggingface.co/PaddlePaddle/PP-OCRv6_small_det_onnx/tree/28fe5895c24fd108c19eb3e8479f4ab385fbfc62)与[识别固定版本](https://huggingface.co/PaddlePaddle/PP-OCRv6_small_rec_onnx/tree/b8f84f0b80c529de40b4fbb3544b84fa7233a513) |
| 文字方向模型 | 官方 [PP-LCNet 固定版本](https://huggingface.co/PaddlePaddle/PP-LCNet_x0_25_textline_ori_onnx/tree/aea1f18d97338aaf84b463f90192f2820524a00a) |
| 字典与许可 | PaddleOCR 固定提交的 [Tiny 字典](https://github.com/PaddlePaddle/PaddleOCR/blob/b03f46425e8ff4442b268ce449e3eef758146cd4/ppocr/utils/dict/ppocrv6_tiny_dict.txt)、[Small 字典](https://github.com/PaddlePaddle/PaddleOCR/blob/b03f46425e8ff4442b268ce449e3eef758146cd4/ppocr/utils/dict/ppocrv6_dict.txt)和 [LICENSE](https://github.com/PaddlePaddle/PaddleOCR/blob/b03f46425e8ff4442b268ce449e3eef758146cd4/LICENSE) |
| ONNX Runtime | 微软官方 [v1.24.4 Release](https://github.com/microsoft/onnxruntime/releases/tag/v1.24.4)，官方发布 API 的压缩包大小及摘要与清单的上游字段一致 |

五个文字／方向模型的官方模型卡或 API 均声明 Apache-2.0。本次核对了官方发布者、固定修订与声明，没有重新完整下载每个文字模型核验镜像内容，也没有复现训练或转换过程。

表格模型的来源链已于 2026-10-21 核对：

| 环节 | 已核实的来源 |
| --- | --- |
| 上游模型 | 飞桨的 [SLANet_plus 官方模型卡](https://huggingface.co/PaddlePaddle/SLANet_plus)；[PaddleOCR 官方文档](https://github.com/PaddlePaddle/PaddleOCR/blob/main/docs/version3.x/module_usage/table_structure_recognition.md)说明其由百度飞桨视觉团队研发 |
| 当前 ONNX 发布件 | [RapidTable v2.0.0 README](https://github.com/RapidAI/RapidTable/blob/v2.0.0/README.md)列出 `slanet-plus.onnx`，模型来源链接指向 PaddleX，下载链接指向其 ModelScope 模型库 |
| RaiShot 的实际下载 | [RapidAI/RapidTable 的 ModelScope v2.0.0 文件](https://www.modelscope.cn/models/RapidAI/RapidTable/resolve/v2.0.0/slanet-plus.onnx)，不是 Bitiful 镜像 |
| 文件身份 | [RapidTable v2.0.0 模型配置](https://github.com/RapidAI/RapidTable/blob/v2.0.0/rapid_table/default_models.yaml)给出的地址与 SHA256 均和 RaiShot 清单一致；从实际地址重新完整匿名下载，7,758,305 字节及摘要全部匹配 |

飞桨官方模型卡、RapidTable 对应版本的 [LICENSE](https://github.com/RapidAI/RapidTable/blob/v2.0.0/LICENSE)及 ModelScope [模型卡](https://www.modelscope.cn/models/RapidAI/RapidTable/resolve/v2.0.0/README.md)均声明 Apache-2.0。这是对发布者声明的核对，不代表整个 RaiShot 分发包的许可与通知检查已完成。

目前可确认该文件是 RapidTable 官方配置采用的 SLANet_plus ONNX 发布件；尚未找到生成同一文件所用的原始 checkpoint、完整转换命令或明确的个人转换者。文件上传者不等于已证实的转换者，也不把飞桨原始 Paddle 权重当作这份 ONNX 文件的逐字节等价物。

## 当前下载保护

以下选源、代理、校验与临时文件机制适用于 RaiShot 自身下载的识别资源；程序包由 Quicker 内置依赖步骤处理。升级程序前需正常退出旧托盘，再运行新版动作。

文字识别的多来源会并行读取最多 256 KiB 测速，每个来源探测上限 3 秒，按预计完整下载时间选择顺序，优先考虑镜像；来源失败后尝试另一来源。单源表格模型没有这层备用保护。

下载显示已下载与总量，并核验固定大小及 SHA256。文件先写临时文件，验证成功才发布到缓存；失败或取消会清理临时文件，已经完成的资源保留。当前没有单文件断点续传，中断的那个文件需要重新下载。

客户端支持 HTTP 代理环境变量与 Windows 手动 HTTP 代理；PAC 自动代理脚本与 SOCKS 不受支持。首次连接、下载和选源仍会受用户网络影响，不能以开发机已有缓存的运行速度代替首次下载验收。

## 当前连通性复核

2026-10-27 在宁波和海外节点分别完整下载并核验全部文字识别镜像与运行库小包，18 次均为 HTTP 200，大小与 SHA256 全部匹配。宁波访问 Hugging Face 检测模型连接失败，海外同地址成功；当前源码已恢复镜像与官方双源。海外全部官方备用来源及两地表格模型也完整核验通过。节点样本与验证边界见[固定下载来源与缓存](../references/ocr-performance-2026-10-13.md#固定下载来源与缓存2026-10-27)。修复需要随新版程序分发，已安装旧版不会自动更换内嵌清单。

## 历史分发实测

2026-10-21 对真实文件进行了完整匿名 GET，正文丢弃，不借用 OCR 缓存。程序包使用已上传的 0.4.13；模型使用当前固定清单。测速工具为 curl，分别绕过代理和使用本机代理环境；这不是 RabShot 完整空缓存流程的耗时。

| 文件 | 本机绕过代理 | 本机当前代理配置 |
| --- | --- | --- |
| 程序包，20.45 MB | 6.29 秒，完整成功 | 5.58 秒，完整成功 |
| Tiny 识别模型 `rec.onnx`，4.46 MB | 2.75 秒，完整成功 | 2.67 秒，完整成功 |
| 表格结构模型，7.76 MB | 重定向后的 DNS 解析超时，未获得模型正文 | 3.86 秒，完整成功 |

Hugging Face 的 Tiny 识别模型备用源，在本机代理配置下另完成 256 KiB Range 请求，约 1.40 秒；这是连通性小样本，不是完整模型下载测速。

这些结果仅代表本机在当时网络下的样本。表格直连失败时没有收到模型正文，不能把失败样本的速率字段当作模型吞吐，也不能据此判断 ModelScope 在国内下载慢。重定向目标为 `cdn-lfs-cn-1.modelscope.cn`，后续本机 DNS 复查已能解析；一次绕过 HTTP 代理的请求仍不等同于普通国内用户网络。已准备并按固定清单验证表格模型的本地镜像候选，尚未上传。

同日经用户明确授权，改用绍兴、宁波的国内服务器测试相同文件。每台按顺序对三个文件各做三次完整 GET，正文直接丢弃，不使用本机代理或本地模型缓存；共 18 次均返回 HTTP 200，下载字节数精确匹配。

| 文件 | 绍兴三轮中位耗时 | 宁波三轮中位耗时 |
| --- | --- | --- |
| 程序包，20.45 MB | 4.18 秒 | 4.22 秒 |
| Tiny 识别模型 `rec.onnx`，4.46 MB | 1.00 秒 | 0.98 秒 |
| 表格结构模型，7.76 MB | 1.45 秒 | 1.57 秒 |

表格模型的三轮范围分别为绍兴 1.30–1.48 秒、宁波 1.47–1.63 秒。两台均未设置 HTTP 代理变量，curl 显式绕过代理；实际目标的路由核查为公网网卡 `ens18`，宁波虽有 Tailscale 接口，下载目标未走该接口。本轮没有修改服务器配置或在远端保存文件，未重新核验服务器所收模型的 SHA256。

这两个浙江机房节点的样本支持继续使用 ModelScope 原发布源，没有测出必须为速度更换源的问题；不扩大为所有地区、运营商或家庭网络的保证。备用镜像作为冗余措施。0.4.14 程序依赖现已公开上传，约 20.46 MB，完整匿名回下载大小／摘要与本地包一致，V1 实际首次下载及全清单校验通过。仍需用不同账号和空缓存验证程序包发现与首次识别流程。后续若发布镜像，应完整匿名回下载核验，再把已验证地址写入清单。
