# OCR 杂项过滤合成语料

`scenes.json` 是在识别／过滤前按绘图语义标注的场景描述。它不包含真实用户截图，不能用于声称生产准确率。

- 16 个独立场景，8 个开发、8 个留出；同图缩放／模糊变体没有跨集合。
- 覆盖中英正文、小字、低对比／模糊、代码标点、表格短项、数字、图标、纹理、伪字形、深浅背景及独立缩放布局。
- `keep` 是实际文字，包含广告、署名等与主要内容无关的真实文字；过滤器不能推断用户意图后删除它们。
- `drop` 是程序绘制且没有文字语义的图形。如果没有被 OCR 识别出来，不算过滤成功。
- `ignore` 供语义无法可靠标注的区域使用；当前场景不故意加入这种区域。跨 `keep`／`drop` 的同一个 OCR 行自动列为合并歧义，不混入准确率。

生成脚本依赖 Pillow，默认只读取明确的 Windows 字体路径，可用 `--font`、`--mono-font` 指定其它字体。生成 PNG、实际绘图坐标标注、识别原始 JSON 及报告均保留在 `output/ocr-filter/`，不把大批 PNG 放入源码。

```powershell
python Tools/ocr-filter-corpus.py
python Tools/evaluate-ocr-filter.py --self-test
python Tools/evaluate-ocr-filter.py --cli target/debug/raishot-cli.exe --raw-dir output/ocr-filter/raw-final
# 先根据开发集确定规则，再冻结策略并一次运行留出。
python Tools/evaluate-ocr-filter.py --cli target/debug/raishot-cli.exe --split holdout --raw-dir output/ocr-filter/raw-final --freeze-note "已按开发集冻结策略"
# 使用同一批缓存合并报告，不重复运行识别。
python Tools/evaluate-ocr-filter.py --split all --raw-dir output/ocr-filter/raw-final --freeze-note "同一冻结策略，合并已缓存的开发与留出结果" --comparison
```

评估按 OCR 原图行框与预标注区域的覆盖匹配，接受一个 OCR 行对应多个同类区域；不同标签合并、无有效框或未匹配行单独列出。正文误删率的分母是识别出的真实文字行；杂项召回率的分母是识别出的非文字行；精确率只计有真值的被过滤行。另列未识别文字／杂项区域，防止把识别遗漏算作过滤收益。一个装饰区域可能包含多个图形，“区域未识别”只表示整个区域没有任何 OCR 候选，不代表逐个图标的识别覆盖率。

CLI `filter.lines` 是建议排除的原始行索引和原因。开发集可用同一次 CLI 返回的 `filter.baseline_lines` 对比初版与当前规则；留出只报告最终冻结策略。缺少 `filter` 的旧 JSON 只允许 `--cache-only` 缓存，不能被当作已经验证过的过滤输出。

静态 HTML 报告含逐行原文、标注类别、前后框和原因，默认深色并响应系统浅色。可选 `--comparison` 以 Pillow 根据原图坐标生成命中与漏过的数据对照 PNG；字体用 `--report-font` 指定。脚本不读取桌面、不操作剪贴板、不打开窗口，也不编译项目。
