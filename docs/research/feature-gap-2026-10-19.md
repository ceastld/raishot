# 细节功能对照来源（2026-10-19）

当前功能差距见[开发路线](../roadmap.md#细节功能清单2026-10-19)；本文件保留当时的产品对照与研究来源，用于解释选型背景，不代表当前完成状态。

## 来源

- PixPin 官方文档（2026-10-19 访问）：贴图 [基础](https://pixpin.cn/docs/pin/base-use)、[图像](https://pixpin.cn/docs/pin/image)、[文本](https://pixpin.cn/docs/pin/text)、[文件](https://pixpin.cn/docs/pin/file)、[颜色](https://pixpin.cn/docs/pin/color)、[LaTeX](https://pixpin.cn/docs/pin/latex)、[贴图组](https://pixpin.cn/docs/pin/pin-group)；[静态截图](https://pixpin.cn/docs/capture/static-capture)；设置 [贴图](https://pixpin.cn/docs/configuration/pin)、[截图](https://pixpin.cn/docs/configuration/screenshot)；[标注基础](https://pixpin.cn/docs/mark/base-use)、[放大镜](https://pixpin.cn/docs/mark/magnifier)；[会员功能](https://pixpin.cn/docs/start/pro-features)；更新日志 [3.4.3.2](https://pixpin.cn/docs/official-log/3.4.3.2)、[3.5.5.1](https://pixpin.cn/docs/official-log/3.5.5.1)、[测试版 3.6.0.9](https://pixpin.cn/docs/change-log/3.6.0.9)。
- 截图 Pro：`<screenshot-pro-docs>/截图体验需求.md`、`截图竞品分析与体验优化计划.md`、`HTML贴图排版与选择.md`；贴图窗口 `<screenshot-pro>/ImageDrawing/Windows/ImageDisplayWindow.xaml` 及分文件（`.HtmlLive.cs`、`.PinGroups.cs`、`.ClickThrough.cs`）。

## 贴图内容类型

- PixPin `Ctrl+2` 按剪贴板内容分派：图像、文本（纯文本／HTML）、文件、颜色、LaTeX；3.5.5.1 起播放 GIF／WebP 动图（`Alt+~` 贴出选中图片）。
- 文本贴图：HTML 保留网页／Word／WPS／VS Code 的颜色、字号、字体、背景；右键“忽略格式”切纯文本；设置有纯文本最大宽度、内边距、显示 HTML 样式、自适应 DPI、字体、文字色、背景色、默认可选择。3.6.0.9 起 HTML 也可限宽，默认 800。文本选择模式下 Alt 拖动移动，选中文字可拖出。
- 文件贴图：可多文件；右键打开、复制此文件／全部文件、复制路径、在资源管理器中显示；Ctrl 拖出。
- 颜色贴图：识别 HEX、`rgb()`、三数值、颜色英文名；右键转其它格式复制；Ctrl 拖出颜色值。
- LaTeX 贴图：渲染公式；右键导出 SVG、复制 MathML、复制 LaTeX 源码、在 Word 中插入。
- 截图 Pro 的 HTML 贴图用隐藏 WebView2 渲染成位图，再用 DOM 字框做文字选择。RabShot 不能依赖 WebView 运行时，只借鉴其行为约定：最大宽度包含卡片留白；能换行的换行，换不下的整卡等比缩小，不因超宽报错；保留原 HTML 的固定宽度、表格布局、缩进、行距与颜色；选择按视觉行，不把跨行选区合成包围盒；单个文本节点逐字测量设上限；连续滚轮先变换已有图像，停稳约 200ms 再按新倍率重排。

## 贴图窗口与管理

- 标题：PixPin 右键或 F2 设置，显示在窗口底部，设置可选顶部或底部居中。截图 Pro 也是 F2，标题在图片下方的保留区域，不进入复制／保存。RabShot 目前只在工作区记录里有标题。
- PixPin 阴影颜色表示状态：蓝＝激活、灰＝非激活、橙＝锁定、绿＝穿透；分组颜色可替代阴影色。
- PixPin 贴图设置另有：锁定类型、恢复顺序、新贴图位置（截图位置／鼠标／主屏中心／鼠标所在屏中心）、Esc／双击关闭确认、历史数量、默认透明度、缩放与精细缩放步长、透明度步长、穿透时降透明度、滚轮与中键缩放基准（含四角）、边框调整大小开关、重复贴图行为、被替换时行为、标题位置；图像贴图：快捷键复制当前图还是原图、默认检测文本、平滑缩放、缩略时隐藏文本、复制文本排版、标点半角。
- PixPin `Ctrl+D` 销毁（不记历史、不保存）；托盘批量：调回屏幕外贴图、全部缩略／取消缩略、全部保存、关闭全部、隐藏／显示全部、销毁全部；3.5.5.1 增加贴图左／右对齐、“隐藏当前选中以外的贴图”。
- 贴图组：默认组＋自建组，可设颜色；`Ctrl+~` 循环切换并短暂显示组名与“屏上／总数”；右键移动到贴图组；管理窗口可重命名、调色、拖动排序、删除或清空。
- 截图 Pro 菜单还有：显示文本区域边框、渲染方式（位图／矢量）、通用／人像抠图（用户已决定暂不做）。

## 截图、标注、识别、输出

- 选区：Shift 锁 1:1、固定比例按钮、点尺寸文字输入宽高；Shift+方向收缩、Ctrl+方向扩展单边；`<`／`>` 载入历史截图（默认 10 张）；`R`／`Shift+R` 循环最近区域；自定义区域／延时截图（可存预设）；3.5.5.1 多截图模式（普通、自由、折线、多窗口，Ctrl+Tab，合成一张，未覆盖部分透明）；三分法辅助线；选区边框颜色；3.6.0.9 复制鼠标坐标（Alt+C）、圆角上限 100。RabShot 的 R 已用于 OCR，最近区域若做需另选键。
- 标注：放大镜（源区域与放大图分别拖动、1–8 倍、连接线样式、阴影、是否显示下方标注）；会员双击箭头添加附注；序号可从 0 开始、全局计数、箭头样式、Z／X 增减；水印描边；荧光笔默认样式 Shift 切换；智能擦除（会员）；所有工具共用颜色可关闭、色板可编辑。
- 识别：工业条码（Code128、EAN13、UPCA、Code39、DataMatrix、PDF417，会员）；多语种 OCR；公式识别可复制 LaTeX／Typst／AsciiMath；文字转语音；表格识别窗口可编辑单元格。
- 输出：保存 PNG／JPG／BMP／WebP／AVIF／PDF，可写 EXIF；剪贴板“复制图像为文件”解决透明兼容；“贴图并复制到剪贴板”选项。
- 录屏：普通录制可剪首尾、意外退出可恢复、摄像头画中画、导出 WebP。
