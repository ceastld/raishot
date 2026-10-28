# 设置文件改为 JSON（2026-10-18）

## 结论

七个设置文件从手写 INI 改为 serde 读写的 JSON，仍按领域分文件、各自加锁：

| 文件 | 模块 | 结构要点 |
| --- | --- | --- |
| `settings.json` | `preferences.rs` | 平铺字段；枚举存线名字符串 |
| `toolbar.json` | `toolbar_config.rs` | `tools` 为工具 ID 数组，`grouped` |
| `tool-presets.json` | `tool_presets.rs` | `tools.<工具ID>` 一组样式字段，只存与默认不同的工具 |
| `selection-style.json` | `selection_finish_store.rs` | `preset`、`current`、`looks.<样式>`；无槽位的旧文件迁移后省略 `looks` |
| `quick_actions.json` | `quick_action_store.rs` | `actions` 数组；内置项只存差异字段 |
| `save.json` | `save_store.rs` | `save` 规则 + `recent_directories` 数组 |
| `recording.json` | `recording_options.rs` | 平铺字段 |

字段名一律 snake_case，顶层 `schema_version: 1`。

## 公共层 `config_json.rs`

- `JsonFile<T>`：`load` 不加锁不建文件；`read_locked` / `write_locked` 要求调用方持有 `lock()`；`replace` = 加锁 → 复核（损坏备份、旧 INI 迁移）→ 整份写入。写入走同目录临时文件 + `sync_all` + rename。
- 锁仍是 `config_recovery::lock`，锁文件固定为 `<名>.ini.lock`，所以 `x.json` 与迁移前的 `x.ini` 共用同一把锁。
- 损坏判定：非 UTF-8、超出大小上限（沿用 `config_recovery`）、JSON 语法错误或顶层不是对象 → 原字节移到 `.corrupt-*.bak`、提示一次、按默认处理。
- 字段级宽容：`overlay(base, map)` 把文件字段逐项叠到默认值上，嵌套对象逐项合并；某字段类型不符或枚举未知只让该字段保留默认，不连累同文件其它字段。未知字段忽略；更高的 `schema_version` 也按已知字段读取。
- 取值校验仍在各 store：目录必须是绝对路径（`validate_directory`）、JPEG 质量 1–100、帧率吸附档位、样式数值范围与颜色格式、热键组合合法等，规则与旧 INI 解析器相同。

## 迁移

没有 `.json` 但有同名 `.ini` 时，在锁内用旧解析器读出 → 写 JSON → `.ini` 改名为 `.ini.bak`。读取和写入路径都会触发，旧 INI 不会被新 JSON 无声遮住。旧 INI 无法读取时按原规则备份为 `.corrupt-*.bak`。

各模块的旧解析器改名为 `parse_legacy_ini`（标注“下个版本删除”）；旧 INI 编码器只留在 `#[cfg(test)]`，用来生成迁移测试的夹具，届时一并删除。

## 行为差异

- 工具栏、快捷动作、工具预设、选区外观、录屏的保存原来锁忙立即失败，现在与偏好一样短暂重试（约 300ms）后再报错。
- 这些 store 原来遇到坏文件静默用默认，现在统一由 `config_recovery` 备份并提示。
- 手改 `settings.json` 造成截图与剪贴板贴图快捷键冲突时，读入后禁用剪贴板贴图快捷键；旧 INI 迁移仍沿用“缺 `pinHotkey` 才让位”的旧规则。
