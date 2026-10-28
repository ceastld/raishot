//! 选区外观偏好（selection-style.json）；只在输出或关闭时显式保存，不随滚轮写磁盘。
use crate::{
    config_json::JsonFile,
    selection_finish::{
        FinishEffect, FinishLooks, FinishPreset, SelectionFinishStyle, MAX_BORDER_THICKNESS,
        MAX_CORNER_RADIUS, MAX_SHADOW_BLUR,
    },
    Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 32 * 1024;
const SCHEMA_VERSION: u32 = 1;

/// 一次读盘的内容：当前外观与样式，加上五个样式各自记住的外观。
#[derive(Clone, Copy, Default)]
struct Saved {
    style: SelectionFinishStyle,
    preset: FinishPreset,
    looks: FinishLooks,
    /// 文件带各样式槽位（旧 INI 自 version=2 起）；更早的文件当前样式要由开关推断。
    has_looks: bool,
}

#[derive(Clone, Default)]
pub(crate) struct SelectionFinishStore {
    saved: SelectionFinishStyle,
    // 新截图的开关经过 remember 策略准备；这不算用户修改，关闭时不能覆盖磁盘。
    baseline: SelectionFinishStyle,
    saved_preset: FinishPreset,
    baseline_preset: FinishPreset,
    saved_looks: FinishLooks,
    baseline_looks: FinishLooks,
    has_looks: bool,
}
impl SelectionFinishStore {
    pub(crate) fn load() -> Self {
        Self::load_at(style_path())
    }
    /// 缺失、损坏或不可读取时使用默认，不阻断截图。
    pub(crate) fn load_at(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        Self::from_file(read_file(path).unwrap_or_else(|error| {
            crate::config_recovery::report_read_failure(path, &error);
            Saved::default()
        }))
    }
    #[cfg(test)]
    fn from_saved(saved: SelectionFinishStyle, preset: FinishPreset) -> Self {
        Self::from_file(Saved {
            style: saved,
            preset,
            ..Default::default()
        })
    }
    fn from_file(file: Saved) -> Self {
        let saved = file.style.normalized();
        let mut baseline = saved;
        baseline.prepare_for_new_capture();
        Self {
            saved,
            baseline,
            saved_preset: file.preset,
            baseline_preset: file.preset,
            saved_looks: file.looks,
            baseline_looks: file.looks,
            has_looks: file.has_looks,
        }
    }
    pub(crate) fn preset(&self) -> FinishPreset {
        self.saved_preset
    }
    pub(crate) fn looks(&self) -> FinishLooks {
        self.saved_looks
    }
    /// 新配置从当前槽位恢复完整快照；只有旧配置才合成固定效果并按开关归类。
    pub(crate) fn session_appearance(&self) -> (SelectionFinishStyle, FinishPreset, FinishLooks) {
        let mut looks = self.looks();
        let (style, preset) = if self.has_looks {
            let preset = self.preset();
            (
                SelectionFinishStyle {
                    remember_enabled: self.saved.remember_enabled,
                    ..looks.get(preset)
                },
                preset,
            )
        } else {
            let style = self.preset().resolve(self.style());
            (style, FinishPreset::matching(style))
        };
        looks.set(preset, style);
        (style, preset, looks)
    }
    /// 文件编辑以原样开始；不把这次初始化算成用户改了样式。
    pub(crate) fn reset_session_preset(&mut self, preset: FinishPreset) {
        self.baseline_preset = preset;
    }
    /// 加载时把当前外观写回所属槽位，不算用户修改。
    pub(crate) fn reset_session_looks(&mut self, looks: FinishLooks) {
        self.baseline_looks = looks;
    }
    /// 会话关闭时提交外观、样式与各样式记住的外观；只合并本会话改过的项。
    pub(crate) fn save_session(
        &mut self,
        style: SelectionFinishStyle,
        preset: FinishPreset,
        looks: FinishLooks,
        effect: Option<FinishEffect>,
    ) -> Result<bool> {
        self.save_full_at(style_path(), style, preset, looks, effect)
    }
    /// 供新会话初始化。圆角独立保留，阴影与边框开关只在 remember 开启时恢复。
    pub(crate) fn style(&self) -> SelectionFinishStyle {
        let mut style = self.saved;
        style.prepare_for_new_capture();
        style
    }
    /// 文件编辑关闭所有效果时重设会话基线；保留原始偏好，不把初始化算成修改。
    pub(crate) fn reset_session_style(&mut self, style: SelectionFinishStyle) {
        self.baseline = validated(style, self.baseline);
    }
    #[cfg(test)]
    pub(crate) fn save_at(
        &mut self,
        path: impl AsRef<Path>,
        style: SelectionFinishStyle,
    ) -> Result<bool> {
        self.save_with_effect_at(path, style, None)
    }
    #[cfg(test)]
    pub(crate) fn save_with_effect_at(
        &mut self,
        path: impl AsRef<Path>,
        style: SelectionFinishStyle,
        effect: Option<FinishEffect>,
    ) -> Result<bool> {
        let (preset, looks) = (self.baseline_preset, self.baseline_looks);
        self.save_full_at(path, style, preset, looks, effect)
    }
    fn save_full_at(
        &mut self,
        path: impl AsRef<Path>,
        style: SelectionFinishStyle,
        preset: FinishPreset,
        looks: FinishLooks,
        effect: Option<FinishEffect>,
    ) -> Result<bool> {
        let style = validated(style, self.baseline);
        if style == self.baseline && preset == self.baseline_preset && looks == self.baseline_looks
        {
            return Ok(false);
        }
        let json = json_file(path.as_ref());
        // 锁忙只短暂重试；失败仍保留原基线，让调用方报告或稍后关闭时重试。
        let _lock = json.lock()?;
        let file = json
            .read_locked()?
            .map(SelectionFile::saved)
            .unwrap_or_default();
        let (previous, previous_preset) = (file.style, file.preset);
        let mut merged = previous;
        // 样式槽位按整份合并：本会话改过的槽位覆盖，其余保留其它窗口写下的值。
        // 首次迁移还要带上旧当前样式的完整外观；它已成为会话基线，并非用户改过的槽位。
        // 另一个窗口已经写成新配置时，以它的槽位为准，仍只合并本会话修改。
        let mut merged_looks = if file.has_looks {
            file.looks
        } else {
            self.baseline_looks
        };
        for look in FinishPreset::ALL {
            if looks.get(look) != self.baseline_looks.get(look) {
                merged_looks.set(look, looks.get(look));
            }
        }
        let preset_changed = preset != self.baseline_preset;
        let merged_preset = if preset_changed {
            preset
        } else {
            previous_preset
        };
        macro_rules! changed {
            ($($field:ident),+ $(,)?) => {
                $(if style.$field != self.baseline.$field {
                    merged.$field = style.$field;
                })+
            };
        }
        changed!(
            corner_enabled,
            corner_radius,
            shadow_enabled,
            shadow_color,
            shadow_blur,
            shadow_opacity,
            border_enabled,
            border_color,
            border_thickness,
            border_inside,
            remember_enabled,
        );
        if style.shadow_enabled != self.baseline.shadow_enabled
            || style.border_enabled != self.baseline.border_enabled
            || effect.is_some()
            || preset_changed
            || (style.remember_enabled && !self.baseline.remember_enabled)
        {
            // 开关是一个模式意图，须成对提交，不能与旧窗口合并出 both。
            // 使用眼前的实际状态：显式意图也可能是关闭，不强行启用模型。
            merged.shadow_enabled = style.shadow_enabled;
            merged.border_enabled = style.border_enabled;
        }
        if effect == Some(FinishEffect::Shadow)
            && (style.shadow_blur != self.baseline.shadow_blur
                || style.shadow_opacity != self.baseline.shadow_opacity)
        {
            // 联动的两值是一个提交；恰好等于旧基线的那一值也不能混入其它窗口的新值。
            // 仅切模式而未改联动值时，仍保留其它窗口独立调整的参数。
            merged.shadow_blur = style.shadow_blur;
            merged.shadow_opacity = style.shadow_opacity;
        }
        merged = merged.normalized();
        let changed = merged != previous
            || merged_preset != previous_preset
            || merged_looks != file.looks
            || !file.has_looks;
        if changed {
            json.write_locked(&SelectionFile::new(
                merged,
                merged_preset,
                Some(&merged_looks),
            ))?;
        }
        // 合并只更新磁盘。保留本会话的值，避免后续关闭把其它窗口的修改覆盖回去。
        self.saved = style;
        self.baseline = style;
        self.saved_preset = preset;
        self.baseline_preset = preset;
        self.saved_looks = looks;
        self.baseline_looks = looks;
        self.has_looks = true;
        Ok(changed)
    }
}
fn style_path() -> PathBuf {
    crate::preferences::data_dir().join("selection-style.json")
}
fn json_file(path: &Path) -> JsonFile<'_, SelectionFile> {
    JsonFile {
        path,
        max_bytes: MAX_FILE_BYTES,
        schema_version: SCHEMA_VERSION,
        legacy: |text| {
            let saved = parse_legacy_ini(text);
            SelectionFile::new(
                saved.style,
                saved.preset,
                saved.has_looks.then_some(&saved.looks),
            )
        },
    }
}
#[cfg(test)]
fn read_style(path: &Path) -> Result<SelectionFinishStyle> {
    Ok(read_file(path)?.style)
}
fn read_file(path: &Path) -> std::io::Result<Saved> {
    Ok(json_file(path)
        .load()?
        .map(SelectionFile::saved)
        .unwrap_or_default())
}

/// selection-style.json 的磁盘结构。`current` 是当前外观，`looks` 是四个样式槽位；
/// 每个槽位从该样式的出厂外观起读，缺项或无效项不至于变成全关。
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct SelectionFile {
    preset: String,
    current: Map<String, Value>,
    /// 迁移自没有槽位的旧文件时省略，读入后按旧规则由开关推断样式。
    #[serde(skip_serializing_if = "Option::is_none")]
    looks: Option<Map<String, Value>>,
}
/// 一组外观字段；颜色是 8 位十六进制 AARRGGBB。
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct StyleFields {
    corner_enabled: bool,
    corner_radius: f32,
    shadow_enabled: bool,
    shadow_color: String,
    shadow_blur: f32,
    shadow_opacity: f32,
    border_enabled: bool,
    border_color: String,
    border_thickness: f32,
    border_inside: bool,
    remember_enabled: bool,
}
impl Default for StyleFields {
    fn default() -> Self {
        Self::from(SelectionFinishStyle::default())
    }
}
impl From<SelectionFinishStyle> for StyleFields {
    fn from(style: SelectionFinishStyle) -> Self {
        Self {
            corner_enabled: style.corner_enabled,
            corner_radius: style.corner_radius,
            shadow_enabled: style.shadow_enabled,
            shadow_color: format!("{:08x}", style.shadow_color),
            shadow_blur: style.shadow_blur,
            shadow_opacity: style.shadow_opacity,
            border_enabled: style.border_enabled,
            border_color: format!("{:08x}", style.border_color),
            border_thickness: style.border_thickness,
            border_inside: style.border_inside,
            remember_enabled: style.remember_enabled,
        }
    }
}
fn style_map(style: SelectionFinishStyle) -> Map<String, Value> {
    match serde_json::to_value(StyleFields::from(style)) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}
/// 与旧 INI 相同的边界：越界或非有限的数值、无效颜色保留 base 的值。
fn style_from(base: SelectionFinishStyle, map: Map<String, Value>) -> SelectionFinishStyle {
    let fields = crate::config_json::overlay(StyleFields::from(base), map);
    let mut style = base;
    let number =
        |value: f32, max: f32| (value.is_finite() && (0. ..=max).contains(&value)).then_some(value);
    macro_rules! set {
        ($field:ident, $value:expr) => {
            if let Some(value) = $value {
                style.$field = value;
            }
        };
    }
    style.corner_enabled = fields.corner_enabled;
    set!(
        corner_radius,
        number(fields.corner_radius, MAX_CORNER_RADIUS)
    );
    style.shadow_enabled = fields.shadow_enabled;
    set!(shadow_color, argb(&fields.shadow_color));
    set!(shadow_blur, number(fields.shadow_blur, MAX_SHADOW_BLUR));
    set!(shadow_opacity, number(fields.shadow_opacity, 1.));
    style.border_enabled = fields.border_enabled;
    set!(border_color, argb(&fields.border_color));
    set!(
        border_thickness,
        number(fields.border_thickness, MAX_BORDER_THICKNESS)
    );
    style.border_inside = fields.border_inside;
    style.remember_enabled = fields.remember_enabled;
    style
}
impl SelectionFile {
    fn new(style: SelectionFinishStyle, preset: FinishPreset, looks: Option<&FinishLooks>) -> Self {
        Self {
            preset: preset.key().into(),
            current: style_map(style),
            looks: looks.map(|looks| {
                FinishPreset::ALL
                    .into_iter()
                    .filter(|look| *look != FinishPreset::Raw)
                    .map(|look| {
                        (
                            look.key().to_string(),
                            Value::Object(style_map(looks.get(look))),
                        )
                    })
                    .collect()
            }),
        }
    }
    fn saved(self) -> Saved {
        let style = style_from(SelectionFinishStyle::default(), self.current).normalized();
        let has_looks = self.looks.is_some();
        let mut looks = FinishLooks::default();
        for (key, value) in self.looks.into_iter().flatten() {
            let (Some(look), Value::Object(map)) = (FinishPreset::parse(&key), value) else {
                continue;
            };
            if look != FinishPreset::Raw {
                looks.set(look, style_from(look.defaults(), map));
            }
        }
        Saved {
            style,
            preset: FinishPreset::parse(&self.preset)
                .unwrap_or_else(|| FinishPreset::from_legacy(style)),
            looks,
            has_looks,
        }
    }
}
#[cfg(test)]
fn parse(text: &str) -> SelectionFinishStyle {
    parse_legacy_ini(text).style
}
#[derive(Clone, Copy, PartialEq)]
enum Section {
    Selection,
    Look,
    Unknown,
}
/// 旧 selection-style.ini 解析器，只用于迁移；下个版本删除。
fn parse_legacy_ini(text: &str) -> Saved {
    let mut style = SelectionFinishStyle::default();
    let mut preset = None;
    // 每个 `[look.<key>]` 节从该样式的出厂外观起读，缺项不至于变成全关。
    let mut looks: Vec<(FinishPreset, SelectionFinishStyle)> = Vec::new();
    let mut section = Section::Selection;
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if line.starts_with('[') {
            section = match line
                .strip_prefix("[look.")
                .and_then(|rest| rest.strip_suffix(']'))
                .and_then(FinishPreset::parse)
            {
                Some(look) if look != FinishPreset::Raw => {
                    looks.push((look, look.defaults()));
                    Section::Look
                }
                _ if line == "[selection]" => Section::Selection,
                _ => Section::Unknown,
            };
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let target = match section {
            Section::Selection => &mut style,
            Section::Look => match looks.last_mut() {
                Some((_, look)) => look,
                None => continue,
            },
            Section::Unknown => continue,
        };
        let key = key.trim();
        let value = value.trim();
        if key == "preset" {
            if section == Section::Selection {
                preset = FinishPreset::parse(value).or(preset);
            }
            continue;
        }
        macro_rules! set {
            ($field:ident, $value:expr) => {
                if let Some(value) = $value {
                    target.$field = value;
                }
            };
        }
        match key {
            "cornerEnabled" => set!(corner_enabled, value.parse::<bool>().ok()),
            "cornerRadius" => set!(corner_radius, number(value, MAX_CORNER_RADIUS)),
            "shadowEnabled" => set!(shadow_enabled, value.parse::<bool>().ok()),
            "shadowColor" => set!(shadow_color, argb(value)),
            "shadowBlur" => set!(shadow_blur, number(value, MAX_SHADOW_BLUR)),
            "shadowOpacity" => set!(shadow_opacity, number(value, 1.)),
            "borderEnabled" => set!(border_enabled, value.parse::<bool>().ok()),
            "borderColor" => set!(border_color, argb(value)),
            "borderThickness" => set!(border_thickness, number(value, MAX_BORDER_THICKNESS)),
            "borderInside" => set!(border_inside, value.parse::<bool>().ok()),
            "rememberEnabled" => set!(remember_enabled, value.parse::<bool>().ok()),
            _ => {}
        }
    }
    let style = style.normalized();
    let mut saved_looks = FinishLooks::default();
    for (look, value) in &looks {
        saved_looks.set(*look, *value);
    }
    Saved {
        style,
        preset: preset.unwrap_or_else(|| FinishPreset::from_legacy(style)),
        looks: saved_looks,
        has_looks: !looks.is_empty(),
    }
}
#[cfg(test)]
fn encode(style: SelectionFinishStyle, preset: FinishPreset) -> String {
    encode_legacy_ini(style, preset, &FinishLooks::default())
}
#[cfg(test)]
fn encode_legacy_ini(
    style: SelectionFinishStyle,
    preset: FinishPreset,
    looks: &FinishLooks,
) -> String {
    let mut text = format!(
        "version=2\n[selection]\npreset={}\n{}rememberEnabled={}\n",
        preset.key(),
        encode_fields(style),
        style.remember_enabled,
    );
    for look in FinishPreset::ALL {
        if look != FinishPreset::Raw {
            text.push_str(&format!(
                "[look.{}]\n{}",
                look.key(),
                encode_fields(looks.get(look))
            ));
        }
    }
    text
}
#[cfg(test)]
fn encode_fields(style: SelectionFinishStyle) -> String {
    format!(
        concat!(
            "cornerEnabled={}\ncornerRadius={}\n",
            "shadowEnabled={}\nshadowColor={:08x}\nshadowBlur={}\nshadowOpacity={}\n",
            "borderEnabled={}\nborderColor={:08x}\nborderThickness={}\nborderInside={}\n",
        ),
        style.corner_enabled,
        style.corner_radius,
        style.shadow_enabled,
        style.shadow_color,
        style.shadow_blur,
        style.shadow_opacity,
        style.border_enabled,
        style.border_color,
        style.border_thickness,
        style.border_inside,
    )
}
/// 旧 INI 数值字段；下个版本随旧解析器删除。
fn number(value: &str, max: f32) -> Option<f32> {
    value
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite() && (0. ..=max).contains(value))
}
fn argb(value: &str) -> Option<u32> {
    let value = value
        .strip_prefix('#')
        .or_else(|| value.strip_prefix("0x"))
        .unwrap_or(value);
    (value.len() == 8)
        .then(|| u32::from_str_radix(value, 16).ok())
        .flatten()
}
fn validated(
    mut style: SelectionFinishStyle,
    baseline: SelectionFinishStyle,
) -> SelectionFinishStyle {
    // 无效浮点输入不能伪造一次用户修改；有效有限值仍遵循模型的统一边界。
    macro_rules! finite {
        ($($field:ident),+ $(,)?) => {
            $(if !style.$field.is_finite() {
                style.$field = baseline.$field;
            })+
        };
    }
    finite!(corner_radius, shadow_blur, shadow_opacity, border_thickness);
    style.normalized()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection_finish::FinishField;
    use std::{
        fs::OpenOptions,
        sync::atomic::{AtomicU64, Ordering},
    };
    fn fixture() -> SelectionFinishStyle {
        SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 24.5,
            shadow_enabled: true,
            shadow_color: 0x8055_2244,
            shadow_blur: 28.,
            shadow_opacity: 0.55,
            border_enabled: true,
            border_color: 0xcdef_1245,
            border_thickness: 3.5,
            border_inside: true,
            remember_enabled: true,
        }
    }
    fn path() -> PathBuf {
        static TEST_ID: AtomicU64 = AtomicU64::new(0);
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/qa")
            .join(format!(
                "selection-finish-store-{}-{}",
                std::process::id(),
                TEST_ID.fetch_add(1, Ordering::Relaxed)
            ))
            .join("selection-style.json")
    }
    fn cleanup(path: &Path) {
        // 只删除本测试独占的子目录（含锁、迁移和损坏备份），不清理共享 QA 目录。
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn selection_finish_ini_roundtrip_remember_and_unknown_invalid_fields() {
        let expected = fixture();
        assert_eq!(parse(&encode(expected, FinishPreset::Card)), expected);
        assert!(
            parse_legacy_ini(&encode(expected, FinishPreset::Stroke)).preset
                == FinishPreset::Stroke
        );
        let store = SelectionFinishStore::from_saved(expected, FinishPreset::Card);
        assert_eq!(store.style(), expected);
        assert_eq!(store.preset(), FinishPreset::Card);
        // 记忆开关不再受 remember 影响；是否生效由样式决定。
        let mut no_remember = expected;
        no_remember.remember_enabled = false;
        let store = SelectionFinishStore::from_saved(no_remember, FinishPreset::Raw);
        assert_eq!(store.style(), no_remember);
        assert!(store.style().corner_enabled);
        assert_eq!(store.style().corner_radius, 24.5);
        let invalid = parse("\u{feff}version=9\n[selection]\ncornerEnabled=broken\ncornerRadius=NaN\nshadowBlur=inf\nshadowOpacity=-1\nborderThickness=999\nshadowColor=bad\nborderColor=#00000000\nfutureField=anything\n[future]\nshadowEnabled=true\n");
        assert_eq!(
            invalid,
            SelectionFinishStyle {
                border_color: 0,
                ..SelectionFinishStyle::default()
            }
        );
    }
    #[test]
    fn selection_finish_looks_roundtrip_and_legacy_files_have_default_looks() {
        let mut looks = FinishLooks::default();
        let card = SelectionFinishStyle {
            corner_enabled: false,
            shadow_blur: 30.,
            shadow_color: 0xff12_3456,
            ..FinishPreset::Card.defaults()
        };
        looks.set(FinishPreset::Card, card);
        let text = encode_legacy_ini(card, FinishPreset::Card, &looks);
        let saved = parse_legacy_ini(&text);
        assert!(saved.has_looks && saved.preset == FinishPreset::Card);
        assert_eq!(saved.style, card);
        assert_eq!(saved.looks, looks);
        // 缺项的槽位从出厂外观起读；原样节与未知样式节被忽略。
        let partial = parse_legacy_ini(
            "version=2\n[selection]\npreset=window\n[look.window]\nborderThickness=4\n[look.raw]\ncornerEnabled=true\n[look.future]\ncornerRadius=9\n",
        );
        assert!(partial.has_looks && partial.preset == FinishPreset::Window);
        assert_eq!(
            partial.looks.get(FinishPreset::Window),
            SelectionFinishStyle {
                border_thickness: 4.,
                ..FinishPreset::Window.defaults()
            }
        );
        assert_eq!(
            partial.looks.get(FinishPreset::Raw),
            FinishPreset::Raw.defaults()
        );
        // 旧文件：没有槽位，预设名仍按旧规则读出。
        let legacy =
            parse_legacy_ini("version=1\n[selection]\npreset=stroke\nborderEnabled=true\n");
        assert!(!legacy.has_looks && legacy.preset == FinishPreset::Stroke);
        assert_eq!(legacy.looks, FinishLooks::default());
    }
    #[test]
    fn selection_finish_looks_merge_only_slots_changed_by_this_session() {
        let path = path();
        let mut first = SelectionFinishStore::load_at(&path);
        let mut second = SelectionFinishStore::load_at(&path);
        let base = first.style();
        let mut looks = first.looks();
        let card = FinishField::Radius.applied(FinishPreset::Card.defaults(), 30.);
        looks.set(FinishPreset::Card, card);
        assert!(first
            .save_full_at(&path, card, FinishPreset::Card, looks, None)
            .unwrap());
        // 另一个窗口只改描边；卡片槽位保留第一个窗口写下的值。
        let mut other = second.looks();
        let stroke = FinishField::Thickness.applied(FinishPreset::Stroke.defaults(), 6.);
        other.set(FinishPreset::Stroke, stroke);
        assert!(second
            .save_full_at(&path, base, FinishPreset::Card, other, None)
            .unwrap());
        let saved = read_file(&path).unwrap();
        assert!(saved.has_looks && saved.preset == FinishPreset::Card);
        assert_eq!(saved.looks.get(FinishPreset::Card), card);
        assert_eq!(saved.looks.get(FinishPreset::Stroke), stroke);
        // 槽位未变、外观未变时不写盘。
        let mut third = SelectionFinishStore::load_at(&path);
        let (style, preset, looks) = (third.style(), third.preset(), third.looks());
        assert!(!third
            .save_full_at(&path, style, preset, looks, None)
            .unwrap());
        cleanup(&path);
    }
    #[test]
    fn selection_finish_session_reload_keeps_customized_effect_switches() {
        for (preset, style) in [
            (
                FinishPreset::Card,
                SelectionFinishStyle {
                    shadow_enabled: false,
                    ..FinishPreset::Card.defaults()
                },
            ),
            (
                FinishPreset::Card,
                SelectionFinishStyle {
                    border_enabled: true,
                    border_thickness: 5.,
                    ..FinishPreset::Card.defaults()
                },
            ),
            (
                FinishPreset::Stroke,
                SelectionFinishStyle {
                    border_enabled: false,
                    ..FinishPreset::Stroke.defaults()
                },
            ),
        ] {
            let path = path();
            let mut writer = SelectionFinishStore::default();
            let mut looks = FinishLooks::default();
            looks.set(preset, style);
            assert!(writer
                .save_full_at(&path, style, preset, looks, None)
                .unwrap());
            let reloaded = SelectionFinishStore::load_at(&path);
            assert_eq!(reloaded.session_appearance(), (style, preset, looks));
            cleanup(&path);
        }
    }
    #[test]
    fn selection_finish_session_uses_slot_instead_of_merged_selection_fields() {
        let style = SelectionFinishStyle {
            corner_radius: 30.,
            shadow_enabled: false,
            ..FinishPreset::Card.defaults()
        };
        let mut looks = FinishLooks::default();
        looks.set(FinishPreset::Card, style);
        let store = SelectionFinishStore::from_file(Saved {
            style: FinishPreset::Window.defaults(),
            preset: FinishPreset::Card,
            looks,
            has_looks: true,
        });
        assert_eq!(
            store.session_appearance(),
            (style, FinishPreset::Card, looks)
        );
        let raw = SelectionFinishStore::from_file(Saved {
            style,
            preset: FinishPreset::Raw,
            looks,
            has_looks: true,
        });
        assert_eq!(
            raw.session_appearance(),
            (FinishPreset::Raw.defaults(), FinishPreset::Raw, looks)
        );
    }
    #[test]
    fn selection_finish_first_migration_preserves_untouched_legacy_slot() {
        let path = path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = SelectionFinishStyle {
            corner_radius: 24.,
            shadow_color: 0xff12_3456,
            ..FinishPreset::Card.defaults()
        };
        std::fs::write(
            path.with_extension("ini"),
            format!(
                "version=1\n[selection]\npreset=card\n{}",
                encode_fields(legacy)
            ),
        )
        .unwrap();
        let mut store = SelectionFinishStore::load_at(&path);
        let (style, preset, looks) = store.session_appearance();
        assert_eq!(style, legacy);
        store.reset_session_style(style);
        store.reset_session_preset(preset);
        store.reset_session_looks(looks);
        // 单纯加载不写盘；切到另一个样式才迁移，原卡片的参数仍须保留。
        assert!(!store
            .save_full_at(&path, style, preset, looks, None)
            .unwrap());
        assert!(store
            .save_full_at(
                &path,
                looks.get(FinishPreset::Round),
                FinishPreset::Round,
                looks,
                None,
            )
            .unwrap());
        let migrated = SelectionFinishStore::load_at(&path);
        assert!(migrated.has_looks);
        assert_eq!(migrated.looks().get(FinishPreset::Card), legacy);
        assert_eq!(migrated.session_appearance().1, FinishPreset::Round);
        cleanup(&path);
    }
    #[test]
    fn selection_finish_save_merges_changed_fields_and_protects_untouched_windows() {
        let path = path();
        let mut a = SelectionFinishStore::load_at(&path);
        let mut b = SelectionFinishStore::load_at(&path);
        let mut a_style = a.style();
        a_style.corner_enabled = true;
        a_style.corner_radius = 24.;
        assert!(!path.exists());
        assert!(a.save_at(&path, a_style).unwrap());
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!b.save_at(&path, b.style()).unwrap());
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        let mut b_style = b.style();
        b_style.remember_enabled = true;
        b_style.shadow_enabled = true;
        b_style.shadow_blur = 28.;
        b_style.shadow_opacity = 0.55;
        assert!(b.save_at(&path, b_style).unwrap());
        let combined = SelectionFinishStore::load_at(&path).style();
        assert!(combined.corner_enabled && combined.shadow_enabled);
        assert_eq!((combined.corner_radius, combined.shadow_blur), (24., 28.));
        assert!(
            !a.save_at(&path, a_style).unwrap(),
            "旧窗口关闭不能回灌新基线"
        );
        assert!(!b.save_at(&path, b_style).unwrap());
        assert_eq!(SelectionFinishStore::load_at(&path).style(), combined);
        cleanup(&path);
    }
    #[test]
    fn selection_finish_prepare_does_not_dirty_config_and_lock_failure_can_retry() {
        let path = path();
        let mut raw = fixture();
        raw.remember_enabled = false;
        let mut writer = SelectionFinishStore::default();
        assert!(writer.save_at(&path, raw).unwrap());
        let mut session = SelectionFinishStore::load_at(&path);
        let prepared = session.style();
        assert_eq!(prepared, raw);
        assert!(!session.save_at(&path, prepared).unwrap());
        assert_eq!(read_style(&path).unwrap(), raw, "prepare 本身不能写回开关");
        let mut remember = prepared;
        remember.remember_enabled = true;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_extension("ini.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(session.save_at(&path, remember).is_err());
        assert_eq!(read_style(&path).unwrap(), raw);
        drop(lock);
        assert!(session.save_at(&path, remember).unwrap());
        let loaded = SelectionFinishStore::load_at(&path).style();
        assert_eq!(loaded, remember);
        let mut invalid = remember;
        invalid.corner_radius = f32::NAN;
        invalid.shadow_blur = f32::INFINITY;
        invalid.border_thickness = f32::NEG_INFINITY;
        assert!(!session.save_at(&path, invalid).unwrap());
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        assert_eq!(
            SelectionFinishStore::load_at(&path).style(),
            SelectionFinishStyle::default()
        );
        cleanup(&path);
    }
    #[test]
    fn selection_finish_image_baseline_does_not_overwrite_capture_preferences() {
        let path = path();
        let raw = fixture();
        let mut writer = SelectionFinishStore::default();
        assert!(writer.save_at(&path, raw).unwrap());
        let mut image_session = SelectionFinishStore::load_at(&path);
        let mut image_style = image_session.style();
        image_style.corner_enabled = false;
        image_style.shadow_enabled = false;
        image_style.border_enabled = false;
        image_session.reset_session_style(image_style);
        assert!(!image_session.save_at(&path, image_style).unwrap());
        assert_eq!(read_style(&path).unwrap(), raw);
        assert_eq!(image_session.style(), raw, "会话基线不能改变已存偏好");
        image_style.corner_enabled = true;
        image_style.corner_radius = 36.;
        assert!(image_session.save_at(&path, image_style).unwrap());
        let mut expected = raw;
        expected.corner_radius = 36.;
        assert_eq!(read_style(&path).unwrap(), expected);
        assert!(!image_session.save_at(&path, image_style).unwrap());
        assert_eq!(read_style(&path).unwrap(), expected);
        cleanup(&path);
    }
    #[test]
    fn selection_effect_linked_modes_reload_without_losing_colors_radius_or_remember() {
        let path = path();
        let initial = SelectionFinishStyle {
            corner_enabled: true,
            corner_radius: 23.25,
            shadow_color: 0x8055_2244,
            border_color: 0xcdef_1245,
            border_inside: true,
            remember_enabled: true,
            ..SelectionFinishStyle::default()
        };
        let shadow = FinishEffect::Shadow.with_intensity(initial, 50.);
        let mut writer = SelectionFinishStore::load_at(&path);
        assert!(writer.save_at(&path, shadow).unwrap());
        let mut session = SelectionFinishStore::load_at(&path);
        let loaded_shadow = session.style();
        assert_eq!(loaded_shadow, shadow);
        assert_eq!(loaded_shadow.exclusive_effect(), Some(FinishEffect::Shadow));
        assert_eq!(loaded_shadow.shadow_blur, 16.);
        assert!((loaded_shadow.shadow_opacity - 0.45).abs() < 1e-6);
        assert!((FinishEffect::Shadow.intensity(loaded_shadow) - 50.).abs() < 1e-5);

        let border = FinishEffect::Border.with_intensity(loaded_shadow, 60.);
        assert!(session.save_at(&path, border).unwrap());
        let loaded_border = SelectionFinishStore::load_at(&path).style();
        assert_eq!(loaded_border, border);
        assert_eq!(loaded_border.exclusive_effect(), Some(FinishEffect::Border));
        assert!((loaded_border.border_thickness - 5.2).abs() < 1e-6);
        assert!((FinishEffect::Border.intensity(loaded_border) - 60.).abs() < 1e-5);
        assert_eq!(loaded_border.shadow_blur, loaded_shadow.shadow_blur);
        assert_eq!(loaded_border.shadow_opacity, loaded_shadow.shadow_opacity);
        assert_eq!(loaded_border.shadow_color, initial.shadow_color);
        assert_eq!(loaded_border.border_color, initial.border_color);
        assert_eq!(loaded_border.corner_radius, initial.corner_radius);
        assert!(loaded_border.corner_enabled && loaded_border.border_inside);
        assert!(loaded_border.remember_enabled);
        cleanup(&path);
    }
    #[test]
    fn selection_effect_unchanged_close_does_not_create_or_rewrite_preferences() {
        let path = path();
        let mut empty = SelectionFinishStore::load_at(&path);
        assert!(!empty.save_at(&path, empty.style()).unwrap());
        assert!(!path.exists());
        assert!(!path.with_extension("ini.lock").exists());
        assert!(!path.parent().unwrap().exists());

        let mut remembered = fixture();
        remembered.shadow_enabled = false;
        remembered.border_enabled = false;
        let remembered = FinishEffect::Shadow.with_intensity(remembered, 75.);
        let mut writer = SelectionFinishStore::load_at(&path);
        assert!(writer.save_at(&path, remembered).unwrap());
        let before = std::fs::read(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut session = SelectionFinishStore::load_at(&path);
        let unchanged = session.style();
        assert!(!session.save_at(&path, unchanged).unwrap());
        assert!(!session.save_at(&path, unchanged).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        cleanup(&path);
    }
    #[test]
    fn selection_effect_session_keeps_switches_and_linked_parameters() {
        let path = path();
        let initial = SelectionFinishStyle {
            remember_enabled: false,
            shadow_enabled: false,
            border_enabled: false,
            ..fixture()
        };
        let configured = FinishEffect::Border.with_intensity(initial, 85.);
        let mut writer = SelectionFinishStore::load_at(&path);
        assert!(writer.save_at(&path, configured).unwrap());
        let raw = read_style(&path).unwrap();
        assert_eq!(raw, configured);
        assert_eq!(raw.exclusive_effect(), Some(FinishEffect::Border));
        let before = std::fs::read(&path).unwrap();
        let mut next = SelectionFinishStore::load_at(&path);
        let prepared = next.style();
        assert_eq!(prepared, configured);
        assert!(prepared.corner_enabled);
        assert_eq!(prepared.corner_radius, initial.corner_radius);
        assert_eq!(prepared.shadow_color, initial.shadow_color);
        assert_eq!(prepared.border_color, initial.border_color);
        assert_eq!(prepared.shadow_blur, initial.shadow_blur);
        assert_eq!(prepared.shadow_opacity, initial.shadow_opacity);
        assert_eq!(prepared.border_thickness, configured.border_thickness);
        assert!(prepared.border_inside);
        assert!(!next.save_at(&path, prepared).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        cleanup(&path);
    }
    #[test]
    fn selection_effect_concurrent_mode_switch_is_atomic_and_same_mode_last_value_wins() {
        let path = path();
        let initial = SelectionFinishStyle {
            shadow_enabled: false,
            border_enabled: false,
            ..fixture()
        };
        let mut writer = SelectionFinishStore::load_at(&path);
        assert!(writer.save_at(&path, initial).unwrap());
        let mut a = SelectionFinishStore::load_at(&path);
        let mut b = SelectionFinishStore::load_at(&path);
        let mut shadow = FinishEffect::Shadow.with_intensity(a.style(), 75.);
        shadow.shadow_color = 0xff31_579b;
        let border = FinishEffect::Border.with_intensity(b.style(), 40.);
        assert!(a.save_at(&path, shadow).unwrap());
        assert!(b.save_at(&path, border).unwrap());
        let merged = SelectionFinishStore::load_at(&path).style();
        assert_eq!(
            merged.exclusive_effect(),
            Some(FinishEffect::Border),
            "后保存的明确模式选择必须成对写开关，不能拼成 legacy both"
        );
        assert!(!merged.shadow_enabled && merged.border_enabled);
        assert_eq!(merged.shadow_color, shadow.shadow_color);
        assert_eq!(merged.shadow_blur, shadow.shadow_blur);
        assert_eq!(merged.shadow_opacity, shadow.shadow_opacity);
        assert_eq!(merged.corner_radius, initial.corner_radius);
        assert!(merged.remember_enabled && merged.border_inside);

        let mut c = SelectionFinishStore::load_at(&path);
        let mut d = SelectionFinishStore::load_at(&path);
        let mut first = FinishEffect::Border.with_intensity(c.style(), 20.);
        first.border_color = 0xffab_cd12;
        let last = FinishEffect::Border.with_intensity(d.style(), 90.);
        assert!(c.save_at(&path, first).unwrap());
        assert!(d.save_at(&path, last).unwrap());
        let merged = SelectionFinishStore::load_at(&path).style();
        assert_eq!(merged.exclusive_effect(), Some(FinishEffect::Border));
        assert_eq!(merged.border_thickness, last.border_thickness);
        assert_eq!(merged.border_color, first.border_color);
        assert_eq!(merged.shadow_color, shadow.shadow_color);
        assert!(!a.save_at(&path, shadow).unwrap());
        assert!(!b.save_at(&path, border).unwrap());
        assert_eq!(SelectionFinishStore::load_at(&path).style(), merged);
        cleanup(&path);
    }
    #[test]
    fn selection_effect_explicit_linked_change_reasserts_mode_and_toggle_off_stays_off() {
        let path = path();
        let initial = FinishEffect::Shadow.with_intensity(
            SelectionFinishStyle {
                remember_enabled: true,
                ..fixture()
            },
            25.,
        );
        let mut writer = SelectionFinishStore::load_at(&path);
        assert!(writer.save_at(&path, initial).unwrap());
        let mut a = SelectionFinishStore::load_at(&path);
        let mut b = SelectionFinishStore::load_at(&path);
        let border = FinishEffect::Border.with_intensity(b.style(), 60.);
        assert!(b
            .save_with_effect_at(&path, border, Some(FinishEffect::Border))
            .unwrap());
        let linked = FinishEffect::Shadow.with_intensity(a.style(), 75.);
        assert_eq!(linked.shadow_enabled, initial.shadow_enabled);
        assert_eq!(linked.border_enabled, initial.border_enabled);
        assert!(a
            .save_with_effect_at(&path, linked, Some(FinishEffect::Shadow))
            .unwrap());
        let loaded = SelectionFinishStore::load_at(&path).style();
        assert_eq!(loaded.exclusive_effect(), Some(FinishEffect::Shadow));
        assert_eq!(loaded.shadow_blur, linked.shadow_blur);
        assert_eq!(loaded.shadow_opacity, linked.shadow_opacity);
        assert_eq!(loaded.border_thickness, border.border_thickness);

        let mut session = SelectionFinishStore::load_at(&path);
        let disabled = SelectionFinishStyle {
            shadow_enabled: false,
            ..session.style()
        };
        assert!(!disabled.shadow_enabled && !disabled.border_enabled);
        assert!(session
            .save_with_effect_at(&path, disabled, Some(FinishEffect::Shadow))
            .unwrap());
        let loaded = SelectionFinishStore::load_at(&path).style();
        assert_eq!(loaded.exclusive_effect(), None);
        assert!(!loaded.shadow_enabled && !loaded.border_enabled);
        assert_eq!(loaded.shadow_blur, linked.shadow_blur);
        assert_eq!(loaded.shadow_opacity, linked.shadow_opacity);
        cleanup(&path);
    }
    #[test]
    fn selection_effect_unrelated_edits_and_explicit_noop_do_not_take_another_windows_mode() {
        let path = path();
        let initial = FinishEffect::Shadow.with_intensity(
            SelectionFinishStyle {
                remember_enabled: true,
                ..fixture()
            },
            25.,
        );
        let mut writer = SelectionFinishStore::load_at(&path);
        assert!(writer.save_at(&path, initial).unwrap());
        let mut unrelated = SelectionFinishStore::load_at(&path);
        let mut unchanged = SelectionFinishStore::load_at(&path);
        let mut mode = SelectionFinishStore::load_at(&path);
        let border = FinishEffect::Border.with_intensity(mode.style(), 70.);
        assert!(mode
            .save_with_effect_at(&path, border, Some(FinishEffect::Border))
            .unwrap());
        let before = std::fs::read(&path).unwrap();
        let old_style = unchanged.style();
        assert!(!unchanged
            .save_with_effect_at(&path, old_style, Some(FinishEffect::Shadow))
            .unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let mut radius_color = unrelated.style();
        radius_color.corner_radius = 37.5;
        radius_color.shadow_color = 0xff91_2345;
        radius_color.border_color = 0xff12_468a;
        assert!(unrelated
            .save_with_effect_at(&path, radius_color, None)
            .unwrap());
        let loaded = SelectionFinishStore::load_at(&path).style();
        assert_eq!(loaded.exclusive_effect(), Some(FinishEffect::Border));
        assert_eq!(loaded.border_thickness, border.border_thickness);
        assert_eq!(loaded.corner_radius, radius_color.corner_radius);
        assert_eq!(loaded.shadow_color, radius_color.shadow_color);
        assert_eq!(loaded.border_color, radius_color.border_color);
        assert_eq!(loaded.shadow_blur, initial.shadow_blur);
        assert_eq!(loaded.shadow_opacity, initial.shadow_opacity);
        assert!(!unrelated
            .save_with_effect_at(&path, radius_color, None)
            .unwrap());
        assert_eq!(SelectionFinishStore::load_at(&path).style(), loaded);
        cleanup(&path);
    }
    #[test]
    fn selection_effect_linked_shadow_pair_overrides_concurrent_half_changes() {
        for (baseline_blur, baseline_opacity, foreign_blur, foreign_opacity) in
            [(10., 0.575, 10., 0.9), (22., 0.63, 40., 0.63)]
        {
            let path = path();
            let initial = SelectionFinishStyle {
                shadow_blur: baseline_blur,
                shadow_opacity: baseline_opacity,
                border_enabled: false,
                ..fixture()
            };
            let mut writer = SelectionFinishStore::load_at(&path);
            assert!(writer.save_at(&path, initial).unwrap());
            let mut linked_session = SelectionFinishStore::load_at(&path);
            let mut independent_session = SelectionFinishStore::load_at(&path);
            let mut independent = independent_session.style();
            independent.shadow_blur = foreign_blur;
            independent.shadow_opacity = foreign_opacity;
            independent.shadow_color = 0xff91_2345;
            independent.corner_radius = 37.5;
            independent.border_thickness = 7.25;
            assert!(independent_session.save_at(&path, independent).unwrap());

            let linked = FinishEffect::Shadow.with_intensity(linked_session.style(), 75.);
            assert_eq!(linked.shadow_blur, 22.);
            assert!((linked.shadow_opacity - 0.575).abs() < 1e-6);
            assert!(
                linked.shadow_blur == initial.shadow_blur
                    || linked.shadow_opacity == initial.shadow_opacity,
                "每个用例都须有一项联动值恰好等于旧基线"
            );
            assert!(linked_session
                .save_with_effect_at(&path, linked, Some(FinishEffect::Shadow))
                .unwrap());
            let loaded = SelectionFinishStore::load_at(&path).style();
            assert_eq!(loaded.exclusive_effect(), Some(FinishEffect::Shadow));
            assert_eq!(loaded.shadow_blur, linked.shadow_blur);
            assert_eq!(loaded.shadow_opacity, linked.shadow_opacity);
            assert_eq!(loaded.shadow_color, independent.shadow_color);
            assert_eq!(loaded.corner_radius, independent.corner_radius);
            assert_eq!(loaded.border_thickness, independent.border_thickness);
            assert_eq!(loaded.border_color, initial.border_color);
            assert_eq!(loaded.remember_enabled, initial.remember_enabled);
            cleanup(&path);
        }
    }
    #[test]
    fn selection_finish_json_round_trip_and_invalid_fields() {
        let path = path();
        let mut looks = FinishLooks::default();
        let card = SelectionFinishStyle {
            shadow_color: 0xff12_3456,
            shadow_blur: 30.,
            ..FinishPreset::Card.defaults()
        };
        looks.set(FinishPreset::Card, card);
        let file = SelectionFile::new(fixture(), FinishPreset::Card, Some(&looks));
        let json = json_file(&path);
        let lock = json.lock().unwrap();
        json.write_locked(&file).unwrap();
        drop(lock);
        let saved = read_file(&path).unwrap();
        assert!(saved.has_looks && saved.preset == FinishPreset::Card);
        assert_eq!(saved.style, fixture());
        assert_eq!(saved.looks, looks);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"schema_version\": 1") && text.contains("\"8055224"));
        // 手改：越界数值、坏颜色与未知样式逐项回到默认，其它字段保留。
        std::fs::write(
            &path,
            r##"{"preset":"future","current":{"corner_enabled":"yes","corner_radius":-1,"shadow_blur":1e9,"border_color":"#00000000","border_inside":true},"looks":{"window":{"border_thickness":4},"raw":{"corner_enabled":true},"future":{}}}"##,
        )
        .unwrap();
        let saved = read_file(&path).unwrap();
        assert_eq!(
            saved.style,
            SelectionFinishStyle {
                border_color: 0,
                border_inside: true,
                ..SelectionFinishStyle::default()
            }
        );
        assert_eq!(saved.preset, FinishPreset::from_legacy(saved.style));
        assert_eq!(
            saved.looks.get(FinishPreset::Window),
            SelectionFinishStyle {
                border_thickness: 4.,
                ..FinishPreset::Window.defaults()
            }
        );
        assert_eq!(
            saved.looks.get(FinishPreset::Raw),
            FinishPreset::Raw.defaults()
        );
        cleanup(&path);
    }
    #[test]
    fn selection_finish_migrates_legacy_ini_to_json() {
        let path = path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut looks = FinishLooks::default();
        let stroke = SelectionFinishStyle {
            border_thickness: 6.,
            ..FinishPreset::Stroke.defaults()
        };
        looks.set(FinishPreset::Stroke, stroke);
        std::fs::write(
            path.with_extension("ini"),
            encode_legacy_ini(fixture(), FinishPreset::Stroke, &looks),
        )
        .unwrap();
        let store = SelectionFinishStore::load_at(&path);
        assert!(store.has_looks && store.preset() == FinishPreset::Stroke);
        assert_eq!(store.looks(), looks);
        assert_eq!(read_style(&path).unwrap(), fixture());
        assert!(path.exists() && path.with_extension("ini.bak").exists());
        assert!(!path.with_extension("ini").exists());
        // 没有槽位的 version=1 旧文件迁移后仍按旧规则推断样式。
        std::fs::remove_file(&path).unwrap();
        std::fs::write(
            path.with_extension("ini"),
            "version=1\n[selection]\npreset=stroke\nborderEnabled=true\n",
        )
        .unwrap();
        let legacy = read_file(&path).unwrap();
        assert!(!legacy.has_looks && legacy.preset == FinishPreset::Stroke);
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"looks\""));
        cleanup(&path);
    }
}
