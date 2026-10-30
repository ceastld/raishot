//! 翻译设置：上方按功能（文字翻译、译成、公式识别、图片翻译）选用服务，选中即存；
//! 下方管理多个服务配置，展开后编辑并点“保存”。保存都不发送翻译或识别请求。
use super::SettingsView;
use crate::{
    preferences::{Preferences, ServiceProfile},
    settings_widgets::{self as widgets, ButtonKind},
    text_field::{TextField, TextFieldEvent},
    toolbar_icons::{self, Icon},
    translate::{Language, LANGUAGES},
    Result,
};
use gpui::{
    anchored, deferred, div, point, prelude::*, px, relative, rgb, rgba, AnyElement, App,
    AppContext, Context, Corner, Edges, Entity, FocusHandle, FontWeight, KeyDownEvent,
    SharedString, Subscription, Window,
};

/// 需要在线服务的功能；各自记住一个配置 id，空串为关闭。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feature {
    Translate,
    Formula,
}
impl Feature {
    fn key(self) -> &'static str {
        match self {
            Self::Translate => "translate",
            Self::Formula => "formula",
        }
    }
    fn title(self) -> &'static str {
        match self {
            Self::Translate => tr!("文字翻译", "Text translation"),
            Self::Formula => tr!("公式识别", "Formula recognition"),
        }
    }
    fn selected(self, preferences: &Preferences) -> &str {
        match self {
            Self::Translate => &preferences.translate_profile,
            Self::Formula => &preferences.formula_profile,
        }
    }
    fn set(self, preferences: &mut Preferences, id: String) {
        match self {
            Self::Translate => preferences.translate_profile = id,
            Self::Formula => preferences.formula_profile = id,
        }
    }
    /// 公式识别要上传图片，只列支持图像的配置。
    fn accepts(self, profile: &ServiceProfile) -> bool {
        self == Self::Translate || profile.supports_vision()
    }
}

/// 去掉协议头的地址，用于列表说明和自定义配置的默认名称。
fn host(endpoint: &str) -> &str {
    let endpoint = endpoint.trim();
    endpoint
        .strip_prefix("https://")
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint)
}

/// 沿现有 ureq 的公开 URL 解析；不调用 call/send，不解析 DNS 或连接服务。
fn validate_endpoint(endpoint: &str) -> Result<()> {
    let parsed = ureq::request("POST", endpoint).request_url().map_err(|_| {
        tr!(
            "请输入有效的 HTTP 或 HTTPS 接口根地址。",
            "Enter a valid HTTP or HTTPS API base URL."
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.as_url().username().is_empty()
        || parsed.as_url().password().is_some()
        || parsed.as_url().query().is_some()
        || parsed.as_url().fragment().is_some()
    {
        return Err(tr!(
            "接口需为 HTTP 或 HTTPS 根地址，不能包含用户名、密码、查询参数或片段。",
            "Use an HTTP or HTTPS API base URL without credentials, a query, or a fragment."
        )
        .into());
    }
    Ok(())
}

fn next_custom_id(preferences: &Preferences) -> String {
    (1..)
        .map(|n| format!("custom-{n}"))
        .find(|id| preferences.service_profile(id).is_none())
        .unwrap()
}

/// 分组标题，右侧放本组的页面级操作。
fn section_header(
    title: &'static str,
    action: impl IntoElement,
    palette: crate::capture_toolbar::Palette,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .min_h(px(28.))
        .px_1()
        .child(
            div()
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(palette.foreground))
                .child(title),
        )
        .child(action)
}

/// 一项服务配置的编辑草稿；`id` 为空表示新增。
#[derive(Clone, Default, PartialEq, Eq)]
struct ProfileDraft {
    id: String,
    name: String,
    endpoint: String,
    model: String,
    api_key: String,
    /// 未编辑密钥的旧草稿不能覆盖另一窗口刚保存的新密钥。
    initial_api_key: String,
    vision: bool,
}
impl std::fmt::Debug for ProfileDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileDraft")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
impl ProfileDraft {
    fn from_profile(profile: &ServiceProfile) -> Self {
        Self {
            id: profile.id.clone(),
            name: profile.name.clone(),
            endpoint: profile.endpoint.clone(),
            model: profile.model.clone(),
            api_key: profile.api_key.clone(),
            initial_api_key: profile.api_key.clone(),
            vision: profile.vision,
        }
    }
    /// 校验后把这一项写进最新设置的副本，返回副本和配置 id；其它字段原样保留。
    fn candidate(&self, latest: &Preferences) -> Result<(Preferences, String)> {
        if [&self.name, &self.endpoint, &self.model, &self.api_key]
            .iter()
            .any(|value| value.chars().any(char::is_control))
        {
            return Err(tr!(
                "服务设置必须是单行文字，不能含控制字符。",
                "Service settings must be single-line text without control characters."
            )
            .into());
        }
        let (id, existing) = if self.id.is_empty() {
            (next_custom_id(latest), None)
        } else {
            match latest.service_profile(&self.id) {
                Some(profile) => (self.id.clone(), Some(profile.clone())),
                None => {
                    return Err(tr!(
                        "这项服务已在其它窗口删除。",
                        "This service was deleted in another window."
                    )
                    .into())
                }
            }
        };
        let mut profile = existing.unwrap_or_else(|| ServiceProfile {
            id: id.clone(),
            ..ServiceProfile::default()
        });
        profile.model = self.model.trim().into();
        if profile.builtin().is_none() {
            let endpoint = self.endpoint.trim();
            if endpoint.is_empty() {
                return Err(tr!("请填写接口根地址。", "Enter the API base URL.").into());
            }
            validate_endpoint(endpoint)?;
            if profile.model.is_empty() {
                return Err(tr!("请填写模型名称。", "Enter the model name.").into());
            }
            profile.endpoint = endpoint.into();
            profile.name = match self.name.trim() {
                "" => host(endpoint).trim_end_matches('/').into(),
                name => name.into(),
            };
            profile.vision = self.vision;
        }
        if self.api_key != self.initial_api_key {
            if profile.api_key != self.initial_api_key && profile.api_key != self.api_key.trim() {
                return Err(tr!(
                    "API Key 已在其它窗口更新，请取消后重新展开读取最新设置；本次草稿仍保留。",
                    "The API key changed in another window. Cancel and reopen to load the latest settings; this draft is kept."
                )
                .into());
            }
            profile.set_api_key(self.api_key.trim().to_owned());
        }
        if !profile.api_key.is_ascii() || profile.api_key.chars().any(char::is_whitespace) {
            return Err(tr!(
                "API Key 不能包含空格、控制字符或非 ASCII 字符。",
                "The API key cannot contain spaces, control characters, or non-ASCII characters."
            )
            .into());
        }
        if profile.needs_key() && profile.api_key.is_empty() {
            return Err(if profile.key_unreadable() {
                tr!(
                    "已保存的 API Key 无法解密，请重新填写。",
                    "The saved API key cannot be decrypted. Enter it again."
                )
            } else {
                tr!(
                    "这项服务需要 API Key；保存不会发送请求。",
                    "This service needs an API key. Saving does not send a request."
                )
            }
            .into());
        }
        let mut candidate = latest.clone();
        match candidate.service_profile_mut(&id) {
            Some(slot) => *slot = profile,
            None => candidate.service_profiles.push(profile),
        }
        Ok((candidate, id))
    }
}

/// 图片翻译引擎与百度凭据的草稿，随图片翻译里的“应用”保存。
#[derive(Clone, PartialEq, Eq)]
struct ImageDraft {
    engine: String,
    baidu_app_id: String,
    baidu_api_key: String,
}
impl ImageDraft {
    fn from_preferences(preferences: &Preferences) -> Self {
        Self {
            engine: preferences.image_translate_engine.clone(),
            baidu_app_id: preferences.baidu_app_id.clone(),
            baidu_api_key: preferences.baidu_api_key.clone(),
        }
    }
    fn normalized_engine(&self) -> &str {
        match self.engine.trim() {
            engine @ ("baidu" | "local") => engine,
            _ => "auto",
        }
    }
    fn candidate(&self, latest: &Preferences) -> Result<Preferences> {
        if [&self.baidu_app_id, &self.baidu_api_key]
            .iter()
            .any(|value| value.chars().any(char::is_control))
        {
            return Err(tr!(
                "翻译设置必须是单行文字，不能含控制字符。",
                "Translation settings must be single-line text without control characters."
            )
            .into());
        }
        let mut candidate = latest.clone();
        candidate.image_translate_engine = self.normalized_engine().to_owned();
        candidate.baidu_app_id = self.baidu_app_id.trim().to_owned();
        candidate.baidu_api_key = self.baidu_api_key.trim().to_owned();
        Ok(candidate)
    }
    fn changed(&self, saved: &Preferences) -> bool {
        let saved = Self::from_preferences(saved);
        self.normalized_engine() != saved.normalized_engine()
            || self.baidu_app_id.trim() != saved.baidu_app_id.trim()
            || self.baidu_api_key.trim() != saved.baidu_api_key.trim()
    }
}

/// 其它即时设置沿旧 save 路线时，不能把此窗口持有的旧翻译配置写回去。
pub(super) fn preserve_latest(preferences: &mut Preferences, latest: &Preferences) {
    preferences.preserve_services_from(latest);
    preferences.translate_target = latest.translate_target.clone();
    preferences.image_translate_engine = latest.image_translate_engine.clone();
    preferences.baidu_app_id = latest.baidu_app_id.clone();
    preferences.baidu_api_key = latest.baidu_api_key.clone();
}

/// 已保存目标语言的三种形态：自动（含空）、菜单里的语言、其它自填文字。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target<'a> {
    Auto,
    Known(&'static Language),
    Other(&'a str),
}
impl<'a> Target<'a> {
    fn of(value: &'a str) -> Self {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("auto") {
            Self::Auto
        } else if let Some(language) = crate::translate::language(value) {
            Self::Known(language)
        } else {
            Self::Other(value)
        }
    }
    /// 界面语言里的名称，用于说明与保存提示。
    fn name(self) -> String {
        match self {
            Self::Auto => tr!("自动", "Auto").into(),
            Self::Known(language) => crate::i18n::Text(language.zh, language.en).get().into(),
            Self::Other(value) => value.into(),
        }
    }
}

/// “其它语言”输入：认得的代码或名称归到菜单项，“自动”写 auto，其余原样保存。
fn parse_target(input: &str) -> Option<String> {
    let input: String = input.chars().filter(|c| !c.is_control()).collect();
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    if input.eq_ignore_ascii_case("auto") || input == "自动" {
        return Some("auto".into());
    }
    let known = crate::translate::language(input).or_else(|| {
        LANGUAGES.iter().find(|language| {
            [language.native, language.zh, language.en]
                .iter()
                .any(|name| name.eq_ignore_ascii_case(input))
        })
    });
    Some(known.map_or_else(|| input.to_owned(), |language| language.code.to_owned()))
}

/// 菜单项序号：0 自动，1..=LANGUAGES.len() 两列语言（行优先），最后一项“其它语言…”。
const MENU_OTHER: usize = LANGUAGES.len() + 1;

/// 方向键在菜单里移动高亮；不处理的键返回 None。
fn menu_step(cursor: Option<usize>, key: &str) -> Option<usize> {
    if !matches!(key, "up" | "down" | "left" | "right") {
        return None;
    }
    let last = LANGUAGES.len();
    let Some(cursor) = cursor else {
        return Some(if matches!(key, "up" | "left") {
            MENU_OTHER
        } else {
            0
        });
    };
    Some(match (cursor, key) {
        (0, "down" | "right") => 1,
        (0, _) => 0,
        // 回到最后一行的左列。
        (MENU_OTHER, "up") => last - (last + 1) % 2,
        (MENU_OTHER, _) => MENU_OTHER,
        (index, "right") if index % 2 == 1 && index < last => index + 1,
        (index, "left") if index % 2 == 0 => index - 1,
        (index, "down") if index + 2 <= last => index + 2,
        (_, "down") => MENU_OTHER,
        (index, "up") if index > 2 => index - 2,
        (_, "up") => 0,
        (index, _) => index,
    })
}

pub(super) struct TranslationFields {
    /// 服务配置编辑器的输入框；同一时间只展开一项。
    name: Entity<TextField>,
    endpoint: Entity<TextField>,
    model: Entity<TextField>,
    api_key: Entity<TextField>,
    /// 正在编辑的配置 id；`Some("")` 表示新增。
    editing: Option<String>,
    initial_api_key: String,
    vision: bool,
    /// “其它语言”输入框；回车直接保存目标语言，不进草稿。
    target: Entity<TextField>,
    baidu_app_id: Entity<TextField>,
    baidu_api_key: Entity<TextField>,
    /// 图片翻译引擎草稿（auto / baidu / local），随“应用”保存。
    engine: String,
    /// 目标语言下拉是否展开。
    target_menu: bool,
    /// 下拉打开时持有键盘焦点；高亮项见 `menu_step`。
    menu_focus: FocusHandle,
    menu_cursor: Option<usize>,
    /// “其它语言”输入行是否显示。
    custom_target: bool,
    /// 展开中的功能服务下拉。
    service_menu: Option<Feature>,
    image_open: bool,
    _subscriptions: Vec<Subscription>,
}
impl TranslationFields {
    pub(super) fn new(
        preferences: &Preferences,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        let name = cx.new(|cx| {
            TextField::new_settings(
                String::new(),
                crate::i18n::Text("如 本机 Ollama", "e.g. Local Ollama"),
                cx,
            )
        });
        let endpoint = cx.new(|cx| {
            TextField::new_settings(
                String::new(),
                crate::i18n::Text("https://example.com/v1", "https://example.com/v1"),
                cx,
            )
        });
        let model = cx.new(|cx| {
            TextField::new_settings(
                String::new(),
                crate::i18n::Text("模型名称", "Model name"),
                cx,
            )
        });
        let target = cx.new(|cx| {
            let text = match Target::of(&preferences.translate_target) {
                Target::Other(value) => value.to_owned(),
                _ => String::new(),
            };
            TextField::new_settings(
                text,
                crate::i18n::Text("如 Svenska、sv、文言文", "e.g. Svenska, sv"),
                cx,
            )
        });
        let api_key = cx.new(|cx| {
            TextField::new_secret_settings(
                String::new(),
                crate::i18n::Text(
                    "填写 API Key（仅本机加密保存）",
                    "API key (encrypted locally)",
                ),
                cx,
            )
        });
        let baidu_app_id = cx.new(|cx| {
            TextField::new_settings(
                preferences.baidu_app_id.clone(),
                crate::i18n::Text("百度翻译开放平台 APP ID", "Baidu Translate APP ID"),
                cx,
            )
        });
        let baidu_api_key = cx.new(|cx| {
            TextField::new_secret_settings(
                preferences.baidu_api_key.clone(),
                crate::i18n::Text("图片翻译 V2.0 API Key", "Picture Translation V2.0 API Key"),
                cx,
            )
        });
        // Tab 顺序与页面自上而下一致。
        let fields = [
            &target,
            &baidu_app_id,
            &baidu_api_key,
            &name,
            &endpoint,
            &model,
            &api_key,
        ];
        let group: Vec<_> = fields.iter().map(|field| field.downgrade()).collect();
        let mut subscriptions = Vec::new();
        for field in fields {
            field.update(cx, |view, cx| {
                view.configure_focus(group.clone(), window, cx)
            });
            subscriptions.push(cx.subscribe_in(
                field,
                window,
                |view: &mut SettingsView, field, event, _, cx| match event {
                    TextFieldEvent::Submit if *field == view.translation_fields.target => {
                        view.save_custom_target(cx)
                    }
                    TextFieldEvent::Submit
                        if *field == view.translation_fields.baidu_app_id
                            || *field == view.translation_fields.baidu_api_key =>
                    {
                        view.apply_image(cx)
                    }
                    TextFieldEvent::Submit => view.save_profile(cx),
                    TextFieldEvent::Changed => {
                        view.status.clear();
                        cx.notify();
                    }
                },
            ));
            // 普通输入器 unmark 只 notify；同样重绘宿主，IME 门不依赖 Changed。
            subscriptions
                .push(cx.observe_in(field, window, |_: &mut SettingsView, _, _, cx| cx.notify()));
        }
        Self {
            name,
            endpoint,
            model,
            api_key,
            editing: None,
            initial_api_key: String::new(),
            vision: false,
            target,
            baidu_app_id,
            baidu_api_key,
            engine: preferences.image_translate_engine.clone(),
            target_menu: false,
            menu_focus: cx.focus_handle(),
            menu_cursor: None,
            custom_target: false,
            service_menu: None,
            image_open: false,
            _subscriptions: subscriptions,
        }
    }
    fn read_profile(&self, cx: &App) -> ProfileDraft {
        ProfileDraft {
            id: self.editing.clone().unwrap_or_default(),
            name: self.name.read(cx).text().to_owned(),
            endpoint: self.endpoint.read(cx).text().to_owned(),
            model: self.model.read(cx).text().to_owned(),
            api_key: self.api_key.read(cx).text().to_owned(),
            initial_api_key: self.initial_api_key.clone(),
            vision: self.vision,
        }
    }
    fn read_image(&self, cx: &App) -> ImageDraft {
        ImageDraft {
            engine: self.engine.clone(),
            baidu_app_id: self.baidu_app_id.read(cx).text().to_owned(),
            baidu_api_key: self.baidu_api_key.read(cx).text().to_owned(),
        }
    }
    pub(super) fn is_composing(&self, cx: &App) -> bool {
        [
            &self.name,
            &self.endpoint,
            &self.model,
            &self.api_key,
            &self.target,
            &self.baidu_app_id,
            &self.baidu_api_key,
        ]
        .iter()
        .any(|field| field.read(cx).is_composing())
    }
    /// 展开一项配置（`id` 为空即新增），输入框换成它的已保存值。
    fn open_editor(&mut self, id: &str, preferences: &Preferences, cx: &mut Context<SettingsView>) {
        let draft = preferences
            .service_profile(id)
            .map(ProfileDraft::from_profile)
            .unwrap_or_default();
        self.editing = Some(draft.id.clone());
        self.initial_api_key = draft.initial_api_key;
        self.vision = draft.vision;
        for (field, text) in [
            (&self.name, draft.name),
            (&self.endpoint, draft.endpoint),
            (&self.model, draft.model),
            (&self.api_key, draft.api_key),
        ] {
            field.update(cx, |view, cx| view.set_text(text, cx));
        }
    }
    fn reset_image(&mut self, preferences: &Preferences, cx: &mut Context<SettingsView>) {
        let draft = ImageDraft::from_preferences(preferences);
        self.engine = draft.engine;
        for (field, text) in [
            (&self.baidu_app_id, draft.baidu_app_id),
            (&self.baidu_api_key, draft.baidu_api_key),
        ] {
            field.update(cx, |view, cx| view.set_text(text, cx));
        }
    }
}

impl SettingsView {
    fn translation_controls_ready(&mut self, cx: &mut Context<Self>) -> bool {
        if self.translation_fields.is_composing(cx) {
            self.status = tr!(
                "请先确认或取消输入法候选，再继续操作。",
                "Confirm or cancel the IME candidate first."
            )
            .into();
            cx.notify();
            false
        } else {
            true
        }
    }
    /// 功能选用哪项服务即选即存，只改这一项。
    fn select_feature_profile(&mut self, feature: Feature, id: String, cx: &mut Context<Self>) {
        self.translation_fields.service_menu = None;
        let latest = Preferences::load();
        self.preferences = latest.clone();
        feature.set(&mut self.preferences, id);
        if !self.save_preferences() {
            self.preferences = latest;
            cx.notify();
            return;
        }
        self.status = match self
            .preferences
            .service_profile(feature.selected(&self.preferences))
        {
            Some(profile) => trf!(
                "已保存：{}使用 {}，下次使用时生效。",
                "Saved: {} uses {} from the next use.",
                feature.title(),
                profile.display_name()
            ),
            None => trf!("已关闭{}。", "{} is off.", feature.title()),
        };
        cx.notify();
    }
    fn toggle_editor(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.translation_controls_ready(cx) {
            return;
        }
        if self.translation_fields.editing.as_deref() == Some(id) {
            self.translation_fields.editing = None;
        } else {
            let latest = Preferences::load();
            self.translation_fields.open_editor(id, &latest, cx);
            preserve_latest(&mut self.preferences, &latest);
        }
        self.status.clear();
        cx.notify();
    }
    fn save_profile(&mut self, cx: &mut Context<Self>) {
        if self.translation_fields.editing.is_none() || !self.translation_controls_ready(cx) {
            return;
        }
        let draft = self.translation_fields.read_profile(cx);
        let latest = Preferences::load();
        self.status = match draft
            .candidate(&latest)
            .and_then(|(candidate, id)| candidate.save().map(|()| (candidate, id)))
        {
            Ok((saved, id)) => {
                let name = saved
                    .service_profile(&id)
                    .map(ServiceProfile::display_name)
                    .unwrap_or_default();
                self.preferences = saved;
                self.translation_fields.editing = None;
                trf!("已保存服务“{}”。", "Saved service “{}”.", name)
            }
            Err(error) => trf!("服务未保存：{error}", "Service not saved: {error}"),
        };
        cx.notify();
    }
    /// 正在被功能选用的配置不能删，先换成别的服务。
    fn delete_profile(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.translation_controls_ready(cx) {
            return;
        }
        let mut latest = Preferences::load();
        let Some(profile) = latest.service_profile(&id).cloned() else {
            self.translation_fields.editing = None;
            cx.notify();
            return;
        };
        if let Some(feature) = [Feature::Translate, Feature::Formula]
            .into_iter()
            .find(|feature| feature.selected(&latest) == id)
        {
            self.status = trf!(
                "“{}”正用于{}，请先在上方换成其它服务。",
                "“{}” is used for {}. Choose another service above first.",
                profile.display_name(),
                feature.title()
            );
            cx.notify();
            return;
        }
        latest.service_profiles.retain(|profile| profile.id != id);
        self.status = match latest.save() {
            Ok(()) => {
                self.preferences = latest;
                self.translation_fields.editing = None;
                trf!(
                    "已删除服务“{}”。",
                    "Deleted service “{}”.",
                    profile.display_name()
                )
            }
            Err(error) => trf!("服务未删除：{error}", "Service not deleted: {error}"),
        };
        cx.notify();
    }
    fn apply_image(&mut self, cx: &mut Context<Self>) {
        if !self.translation_controls_ready(cx) {
            return;
        }
        let draft = self.translation_fields.read_image(cx);
        let latest = Preferences::load();
        self.status = match draft
            .candidate(&latest)
            .and_then(|candidate| candidate.save().map(|()| candidate))
        {
            Ok(saved) => {
                self.translation_fields.reset_image(&saved, cx);
                self.preferences = saved;
                tr!(
                    "图片翻译设置已保存，下次翻译生效。",
                    "Image translation settings saved for the next translation."
                )
                .into()
            }
            Err(error) => trf!(
                "翻译设置未保存：{error}",
                "Translation settings not saved: {error}"
            ),
        };
        cx.notify();
    }
    fn cancel_image_changes(&mut self, cx: &mut Context<Self>) {
        if !self.translation_controls_ready(cx) {
            return;
        }
        let latest = Preferences::load();
        self.translation_fields.reset_image(&latest, cx);
        preserve_latest(&mut self.preferences, &latest);
        self.status.clear();
        cx.notify();
    }
    /// 功能选择、目标语言与图片引擎立即恢复默认；服务配置和已填的 Key 保留。
    fn reset_translation_defaults(&mut self, cx: &mut Context<Self>) {
        if !self.translation_controls_ready(cx) {
            return;
        }
        let latest = Preferences::load();
        let defaults = Preferences::default();
        self.preferences = latest.clone();
        self.preferences.translate_profile = defaults.translate_profile;
        self.preferences.formula_profile = defaults.formula_profile;
        self.preferences.translate_target = defaults.translate_target;
        self.preferences.image_translate_engine = defaults.image_translate_engine;
        if !self.save_preferences() {
            self.preferences = latest;
            cx.notify();
            return;
        }
        let fields = &mut self.translation_fields;
        fields.custom_target = false;
        fields.service_menu = None;
        let saved = self.preferences.clone();
        self.translation_fields.reset_image(&saved, cx);
        self.status = tr!(
            "已恢复默认：翻译用 Index，公式用 DeepSeek，译成自动；服务配置与 Key 保留。",
            "Defaults restored: Index for translation, DeepSeek for formulas, target Auto. Services and keys are kept."
        )
        .into();
        cx.notify();
    }
    /// 与主题、语言一样只改这一项并立即保存；不碰服务草稿。
    fn save_translation_target(&mut self, target: String) -> bool {
        let latest = Preferences::load();
        self.preferences = latest.clone();
        self.preferences.translate_target = target;
        if !self.save_preferences() {
            self.preferences = latest;
            return false;
        }
        let target = Target::of(&self.preferences.translate_target);
        self.status = match target {
            Target::Auto => tr!(
                "已保存：自动选择中英文，下次翻译生效。",
                "Saved: Auto (zh ⇄ en), used for the next translation."
            )
            .into(),
            _ => trf!(
                "已保存：译成{}，下次翻译生效。",
                "Saved: translate into {}, used for the next translation.",
                target.name()
            ),
        };
        true
    }
    fn pick_target(&mut self, target: &str, cx: &mut Context<Self>) {
        self.translation_fields.target_menu = false;
        self.translation_fields.custom_target = false;
        self.save_translation_target(target.to_owned());
        cx.notify();
    }
    /// 菜单键盘：方向键在两列间移动，回车／空格选中，Esc 关闭且不改值。
    fn target_menu_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let fields = &mut self.translation_fields;
        match (key, fields.menu_cursor) {
            ("escape", _) => fields.target_menu = false,
            ("enter" | "space", Some(0)) => return self.pick_target("auto", cx),
            ("enter" | "space", Some(MENU_OTHER)) => return self.open_custom_target(window, cx),
            ("enter" | "space", Some(index)) => {
                if let Some(language) = LANGUAGES.get(index - 1) {
                    self.pick_target(language.code, cx);
                }
                return;
            }
            _ => match menu_step(fields.menu_cursor, key) {
                Some(cursor) => fields.menu_cursor = Some(cursor),
                None => return,
            },
        }
        cx.stop_propagation();
        cx.notify();
    }
    fn open_custom_target(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fields = &mut self.translation_fields;
        fields.target_menu = false;
        fields.custom_target = true;
        let text = match Target::of(&self.preferences.translate_target) {
            Target::Other(value) => value.to_owned(),
            _ => String::new(),
        };
        fields
            .target
            .update(cx, |field, cx| field.set_text(text, cx));
        window.focus(&fields.target.read(cx).focus_handle);
        cx.notify();
    }
    fn save_custom_target(&mut self, cx: &mut Context<Self>) {
        if !self.translation_controls_ready(cx) {
            return;
        }
        match parse_target(self.translation_fields.target.read(cx).text()) {
            Some(target) => {
                if self.save_translation_target(target) {
                    self.translation_fields.custom_target = false;
                }
            }
            None => {
                self.status = tr!("请输入语言名称或代码。", "Enter a language name or code.").into()
            }
        }
        cx.notify();
    }
    fn translation_field(&self, id: &'static str, field: &Entity<TextField>) -> AnyElement {
        div()
            .id(id)
            .relative()
            .w_full()
            .child(field.clone())
            .child(self.settings_probe(id))
            .into_any_element()
    }
    pub(super) fn translation_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let fields = &self.translation_fields;
        let image_changed = fields.read_image(cx).changed(&self.preferences);
        let image = div()
            .w_full()
            .flex()
            .flex_col()
            .child(self.image_row(cx))
            .when(fields.image_open || image_changed, |row| {
                row.child(self.image_panel(cx))
            })
            .when(image_changed, |row| {
                row.child(
                    div()
                        .m_1()
                        .px_3()
                        .py(px(10.))
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .rounded_md()
                        .bg(rgba((palette.accent << 8) | 0x24))
                        .child(div().flex_1().min_w(px(140.)).text_size(px(13.)).child(tr!(
                            "图片翻译有改动，应用后下次翻译生效。",
                            "Image translation changes take effect after Apply."
                        )))
                        .child(
                            widgets::button(
                                "settings-translation-cancel",
                                tr!("取消修改", "Cancel"),
                                ButtonKind::Secondary,
                                palette,
                                surface,
                            )
                            .child(self.settings_probe("settings-translation-cancel"))
                            .on_click(cx.listener(|view, _, _, cx| view.cancel_image_changes(cx))),
                        )
                        .child(
                            widgets::button(
                                "settings-translation-apply",
                                tr!("应用", "Apply"),
                                ButtonKind::Primary,
                                palette,
                                surface,
                            )
                            .child(self.settings_probe("settings-translation-apply"))
                            .on_click(cx.listener(|view, _, _, cx| view.apply_image(cx))),
                        ),
                )
            });
        let mut features = vec![
            self.feature_row(Feature::Translate, cx).into_any_element(),
            self.target_row(cx).into_any_element(),
        ];
        if fields.custom_target {
            features.push(self.custom_target_row(cx).into_any_element());
        }
        features.push(self.feature_row(Feature::Formula, cx).into_any_element());
        features.push(image.into_any_element());
        let restore = div()
            .id("settings-translation-defaults")
            .relative()
            .px_1()
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(rgb(palette.accent))
            .hover(|style| style.opacity(0.8))
            .child(tr!("恢复默认", "Restore defaults"))
            .child(self.settings_probe("settings-translation-defaults"))
            .on_click(cx.listener(|view, _, _, cx| view.reset_translation_defaults(cx)));
        let uses = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_2()
            .child(section_header(
                tr!("各功能使用", "Features"),
                restore,
                palette,
            ))
            .child(widgets::card(surface, features).w_full());
        let mut profiles: Vec<AnyElement> = Vec::new();
        for profile in &self.preferences.service_profiles {
            let open = fields.editing.as_deref() == Some(profile.id.as_str());
            profiles.push(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(self.profile_row(profile, open, cx))
                    .when(open, |row| {
                        row.child(self.profile_editor(Some(profile), cx))
                    })
                    .into_any_element(),
            );
        }
        if fields.editing.as_deref() == Some("") {
            profiles.push(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(widgets::row(
                        tr!("新服务", "New service"),
                        Some(
                            tr!(
                                "OpenAI 兼容接口，如本机 Ollama、LM Studio 或其它在线服务。",
                                "Any OpenAI-compatible API, such as local Ollama, LM Studio, or another online service."
                            )
                            .into(),
                        ),
                        palette,
                    ))
                    .child(self.profile_editor(None, cx))
                    .into_any_element(),
            );
        }
        let add = widgets::button(
            "settings-translation-profile-add",
            tr!("添加服务", "Add service"),
            ButtonKind::Secondary,
            palette,
            surface,
        )
        .h(px(28.))
        .text_size(px(12.))
        .child(self.settings_probe("settings-translation-profile-add"))
        .on_click(cx.listener(|view, _, _, cx| view.toggle_editor("", cx)));
        let services = div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_2()
            .child(section_header(tr!("服务配置", "Services"), add, palette))
            .child(widgets::card(surface, profiles).w_full());
        let footer = div().w_full().child(
            widgets::note(
                tr!(
                    "翻译只发送所选文字；公式识别只发送选区图片；表格识别在本机。Key 仅当前 Windows 用户可解密。",
                    "Translation sends only the selected text; formulas send only the selection image; tables stay local. Keys are encrypted for this Windows user."
                ),
                palette,
            )
            .w_full(),
        );
        div()
            .w_full()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_4()
            .child(uses)
            .child(services)
            .child(footer)
            .into_any_element()
    }
    /// 功能行：右侧下拉选择服务；所选服务缺 Key 时说明里直接提示。
    fn feature_row(&self, feature: Feature, cx: &mut Context<Self>) -> gpui::Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let saved = &self.preferences;
        let selected = saved.service_profile(feature.selected(saved));
        let missing_key =
            selected.is_some_and(|profile| profile.needs_key() && profile.api_key.is_empty());
        let description: SharedString = match (selected, feature) {
            (None, Feature::Translate) => tr!(
                "已关闭；OCR 结果不再显示翻译入口。",
                "Off; OCR results no longer offer translation."
            )
            .into(),
            (None, Feature::Formula) => tr!(
                "已关闭；表格识别在本机，不受影响。",
                "Off; local table recognition is unaffected."
            )
            .into(),
            (Some(profile), _) if profile.key_unreadable() => tr!(
                "API Key 无法解密，请在下方服务配置中重新填写。",
                "The API key cannot be decrypted. Enter it again under Services below."
            )
            .into(),
            (Some(_), _) if missing_key => tr!(
                "还没有填写 API Key，请在下方服务配置中填写。",
                "No API key yet. Enter it under Services below."
            )
            .into(),
            (Some(profile), Feature::Translate) => trf!(
                "{} · 只发送所选文字。",
                "{} · Sends only the selected text.",
                profile.effective_model()
            )
            .into(),
            (Some(profile), Feature::Formula) => trf!(
                "{} · 只发送选区图片。",
                "{} · Sends only the selection image.",
                profile.effective_model()
            )
            .into(),
        };
        let open = self.translation_fields.service_menu == Some(feature);
        let label: SharedString = selected
            .map_or_else(
                || tr!("关闭", "Off").to_owned(),
                ServiceProfile::display_name,
            )
            .into();
        let trigger_id = format!("settings-translation-{}-service", feature.key());
        let trigger = div()
            .id(SharedString::from(trigger_id.clone()))
            .relative()
            .h(px(32.))
            .w(px(220.))
            .pl_3()
            .pr_2()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(if open {
                palette.accent
            } else if missing_key {
                palette.danger
            } else {
                palette.border
            }))
            .bg(rgb(surface.raised))
            .text_size(px(13.))
            .cursor_pointer()
            .hover(move |style| style.bg(rgb(palette.hover)))
            .on_click(cx.listener(move |view, _, _, cx| {
                let fields = &mut view.translation_fields;
                fields.target_menu = false;
                fields.service_menu = (fields.service_menu != Some(feature)).then_some(feature);
                cx.notify();
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .when(selected.is_none(), |label| {
                        label.text_color(rgb(palette.muted))
                    })
                    .child(label),
            )
            .child(
                toolbar_icons::action(
                    if open { Icon::ChevronUp } else { Icon::Chevron },
                    palette.muted,
                )
                .size(px(14.)),
            )
            .child(self.settings_probe(trigger_id));
        let control = div()
            .relative()
            .flex_shrink_0()
            .child(trigger)
            .when(open, |control| {
                control
                    .child(deferred(
                        anchored()
                            .position(point(px(0.), px(0.)))
                            .snap_to_window()
                            .child(
                                div()
                                    .id("settings-translation-service-backdrop")
                                    .w(px(8000.))
                                    .h(px(8000.))
                                    .occlude()
                                    .on_any_mouse_down(cx.listener(|view, _, _, cx| {
                                        view.translation_fields.service_menu = None;
                                        cx.notify();
                                    })),
                            ),
                    ))
                    .child(
                        div().absolute().right_0().bottom_0().child(
                            deferred(
                                anchored()
                                    .anchor(Corner::TopRight)
                                    .offset(point(px(0.), px(4.)))
                                    .snap_to_window_with_margin(Edges::all(px(8.)))
                                    .child(self.service_menu(feature, cx)),
                            )
                            .with_priority(1),
                        ),
                    )
            });
        widgets::row(feature.title(), Some(description), palette).child(control)
    }
    fn service_menu(&self, feature: Feature, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let menu_bg = if crate::theme::is_light(cx) {
            0xffffff
        } else {
            0x262d36
        };
        let current = feature.selected(&self.preferences).to_owned();
        let mut options: Vec<(String, SharedString, Option<String>)> = self
            .preferences
            .service_profiles
            .iter()
            .filter(|profile| feature.accepts(profile))
            .map(|profile| {
                (
                    profile.id.clone(),
                    profile.display_name().into(),
                    Some(profile.effective_model().to_owned()),
                )
            })
            .collect();
        options.push((String::new(), tr!("关闭", "Off").into(), None));
        div()
            .id(SharedString::from(format!(
                "settings-translation-{}-menu",
                feature.key()
            )))
            .occlude()
            .w(px(260.))
            .p_1()
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(rgb(palette.border))
            .bg(rgb(menu_bg))
            .shadow_lg()
            .children(options.into_iter().map(|(id, name, model)| {
                let selected = id == current;
                let probe = format!(
                    "settings-translation-{}-option-{}",
                    feature.key(),
                    if id.is_empty() { "off" } else { id.as_str() }
                );
                div()
                    .id(SharedString::from(probe.clone()))
                    .relative()
                    .min_h(px(32.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(13.))
                    .hover(move |style| style.bg(rgb(palette.hover)))
                    .child(div().size(px(14.)).flex_shrink_0().when(selected, |slot| {
                        slot.child(
                            toolbar_icons::action(Icon::Confirm, palette.accent).size(px(14.)),
                        )
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .when(selected, |label| {
                                label
                                    .text_color(rgb(palette.accent))
                                    .font_weight(FontWeight::SEMIBOLD)
                            })
                            .child(name),
                    )
                    .when_some(model, |item, model| {
                        item.child(
                            div()
                                .flex_shrink_0()
                                .max_w(px(110.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(12.))
                                .text_color(rgb(palette.muted))
                                .child(model),
                        )
                    })
                    .child(self.settings_probe(probe))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.select_feature_profile(feature, id.clone(), cx)
                    }))
            }))
    }
    /// 配置列表的一行：名称、模型与能力、Key 状态及被哪些功能选用；点击展开编辑。
    fn profile_row(
        &self,
        profile: &ServiceProfile,
        open: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let capability = if profile.supports_vision() {
            tr!("文字、图像", "Text, images")
        } else {
            tr!("文字", "Text")
        };
        let mut description = format!("{} · {capability}", profile.effective_model());
        if profile.builtin().is_none() {
            description.push_str(" · ");
            description.push_str(host(&profile.endpoint));
        }
        let (status, warn): (&str, bool) = if profile.key_unreadable() {
            (tr!("Key 无法解密", "Key unreadable"), true)
        } else if !profile.api_key.is_empty() {
            (tr!("Key 已保存", "Key saved"), false)
        } else if profile.needs_key() {
            (tr!("缺 Key", "Needs key"), true)
        } else {
            (tr!("免 Key", "No key"), false)
        };
        let uses: Vec<&str> = [Feature::Translate, Feature::Formula]
            .into_iter()
            .filter(|feature| feature.selected(&self.preferences) == profile.id)
            .map(|feature| match feature {
                Feature::Translate => tr!("翻译", "Translation"),
                Feature::Formula => tr!("公式", "Formulas"),
            })
            .collect();
        let id = profile.id.clone();
        let probe = format!("settings-translation-profile-{}", profile.id);
        widgets::row(profile.display_name(), Some(description.into()), palette)
            .id(SharedString::from(probe.clone()))
            .cursor_pointer()
            .hover(move |style| style.bg(rgb(surface.nav_hover)))
            .when(!uses.is_empty(), |row| {
                row.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(rgb(palette.muted))
                        .child(trf!("用于{}", "For {}", uses.join(tr!("、", ", ")))),
                )
            })
            .child(
                div()
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .rounded_md()
                    .text_size(px(12.))
                    .when(warn, |badge| {
                        badge
                            .bg(rgba(palette.danger_bg))
                            .text_color(rgb(palette.danger))
                    })
                    .when(!warn, |badge| {
                        badge.bg(rgb(surface.track)).text_color(rgb(palette.muted))
                    })
                    .child(status),
            )
            .child(
                toolbar_icons::action(
                    if open { Icon::ChevronUp } else { Icon::Chevron },
                    palette.muted,
                )
                .size(px(16.)),
            )
            .child(self.settings_probe(probe))
            .on_click(cx.listener(move |view, _, _, cx| view.toggle_editor(&id, cx)))
    }
    /// 展开的编辑器：内置服务只改模型和 Key；自定义服务还可改名称、地址和图像能力。
    fn profile_editor(
        &self,
        profile: Option<&ServiceProfile>,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let fields = &self.translation_fields;
        let builtin = profile.and_then(ServiceProfile::builtin);
        let needs_key = profile.is_some_and(ServiceProfile::needs_key);
        let label = |text: &'static str| {
            div()
                .px_1()
                .text_size(px(12.))
                .text_color(rgb(palette.foreground))
                .child(text)
        };
        let mut panel = div().w_full().flex().flex_col().gap_2().px_3().pb(px(14.));
        if builtin.is_none() {
            panel = panel
                .child(label(tr!("名称", "Name")))
                .child(self.translation_field("settings-translation-profile-name", &fields.name))
                .child(label(tr!("接口根地址", "API base URL")))
                .child(self.translation_field("settings-translation-endpoint", &fields.endpoint))
                .child(widgets::note(
                    tr!(
                        "OpenAI 兼容接口，通常以 /v1 结尾。",
                        "OpenAI-compatible API, usually ending in /v1."
                    ),
                    palette,
                ));
        }
        panel = panel
            .child(label(tr!("模型", "Model")))
            .child(self.translation_field("settings-translation-model", &fields.model));
        if let Some(builtin) = builtin {
            panel = panel.child(widgets::note(
                trf!(
                    "留空使用默认模型 {}。",
                    "Leave empty for the default model {}.",
                    builtin.model
                ),
                palette,
            ));
        }
        panel = panel
            .child(label(if needs_key {
                "API Key"
            } else {
                tr!("API Key（可选）", "API key (optional)")
            }))
            .child(self.translation_field("settings-translation-api-key", &fields.api_key))
            .when(profile.is_some_and(ServiceProfile::key_unreadable), |panel| {
                panel.child(widgets::note(
                    tr!(
                        "原 API Key 无法解密，请重新填写；原密文仍保留。",
                        "The saved API key cannot be decrypted. Enter it again; its encrypted value is kept."
                    ),
                    palette,
                ))
            });
        if builtin.is_none() {
            let vision = fields.vision;
            panel = panel.child(
                div()
                    .id("settings-translation-profile-vision")
                    .relative()
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_1()
                    .py_1()
                    .cursor_pointer()
                    .child(widgets::switch(vision, palette, surface))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(13.))
                            .text_color(rgb(palette.foreground))
                            .child(tr!(
                                "支持图像输入（可用于公式识别）",
                                "Accepts images (usable for formulas)"
                            )),
                    )
                    .child(self.settings_probe("settings-translation-profile-vision"))
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.translation_fields.vision = !view.translation_fields.vision;
                        cx.notify();
                    })),
            );
        }
        let mut actions = div().flex().items_center().gap_2().pt_1();
        if let Some(profile) = profile.filter(|profile| profile.builtin().is_none()) {
            let id = profile.id.clone();
            actions = actions.child(
                div()
                    .id("settings-translation-profile-delete")
                    .relative()
                    .px_1()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(palette.danger))
                    .hover(|style| style.opacity(0.8))
                    .child(tr!("删除这项服务", "Delete service"))
                    .child(self.settings_probe("settings-translation-profile-delete"))
                    .on_click(
                        cx.listener(move |view, _, _, cx| view.delete_profile(id.clone(), cx)),
                    ),
            );
        }
        actions = actions
            .child(div().flex_1())
            .child(
                widgets::button(
                    "settings-translation-profile-cancel",
                    tr!("取消", "Cancel"),
                    ButtonKind::Secondary,
                    palette,
                    surface,
                )
                .child(self.settings_probe("settings-translation-profile-cancel"))
                .on_click(cx.listener(|view, _, _, cx| {
                    view.translation_fields.editing = None;
                    view.status.clear();
                    cx.notify();
                })),
            )
            .child(
                widgets::button(
                    "settings-translation-profile-save",
                    tr!("保存", "Save"),
                    ButtonKind::Primary,
                    palette,
                    surface,
                )
                .child(self.settings_probe("settings-translation-profile-save"))
                .on_click(cx.listener(|view, _, _, cx| view.save_profile(cx))),
            );
        panel.child(actions)
    }
    fn target_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let off = self.preferences.translate_profile.is_empty();
        let target = Target::of(&self.preferences.translate_target);
        let open = self.translation_fields.target_menu && !off;
        let description: SharedString = if off {
            tr!(
                "文字翻译已关闭，可在上一行选择服务开启。",
                "Text translation is off. Choose a service in the row above."
            )
            .into()
        } else if target == Target::Auto {
            tr!(
                "中文译成英文，其它语言译成简体中文。",
                "Chinese becomes English; other languages become Simplified Chinese."
            )
            .into()
        } else {
            trf!(
                "所有识别文字都译成{}。",
                "All recognized text is translated into {}.",
                target.name()
            )
            .into()
        };
        let (label, hint): (SharedString, Option<&str>) = match target {
            Target::Auto => (tr!("自动", "Auto").into(), Some(tr!("中英互译", "zh ⇄ en"))),
            Target::Known(language) => (language.native.into(), None),
            Target::Other(value) => (value.to_owned().into(), None),
        };
        let trigger = div()
            .id("settings-translation-target")
            .relative()
            .h(px(32.))
            .min_w(px(168.))
            .max_w(px(240.))
            .pl_3()
            .pr_2()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(rgb(if open { palette.accent } else { palette.border }))
            .bg(rgb(surface.raised))
            .text_size(px(13.))
            .when(!off, |trigger| {
                trigger
                    .cursor_pointer()
                    .hover(move |style| style.bg(rgb(palette.hover)))
                    .on_click(cx.listener(|view, _, window, cx| {
                        let fields = &mut view.translation_fields;
                        fields.service_menu = None;
                        fields.target_menu = !fields.target_menu;
                        fields.menu_cursor = None;
                        if fields.target_menu {
                            window.focus(&fields.menu_focus);
                        }
                        cx.notify();
                    }))
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(div().min_w(px(0.)).text_ellipsis().child(label))
                    .when_some(hint, |line, hint| {
                        line.child(
                            div()
                                .flex_shrink_0()
                                .text_color(rgb(palette.muted))
                                .child(hint),
                        )
                    }),
            )
            .child(
                toolbar_icons::action(
                    if open { Icon::ChevronUp } else { Icon::Chevron },
                    palette.muted,
                )
                .size(px(14.)),
            )
            .child(self.settings_probe("settings-translation-target"));
        let control = div()
            .relative()
            .flex_shrink_0()
            .child(trigger)
            .when(open, |control| {
                control
                    // 透明遮罩接住菜单外的按下：关闭菜单且不穿透到下面的控件。
                    .child(deferred(
                        anchored()
                            .position(point(px(0.), px(0.)))
                            .snap_to_window()
                            .child(
                                div()
                                    .id("settings-translation-target-backdrop")
                                    .w(px(8000.))
                                    .h(px(8000.))
                                    .occlude()
                                    .on_any_mouse_down(cx.listener(|view, _, _, cx| {
                                        view.translation_fields.target_menu = false;
                                        cx.notify();
                                    })),
                            ),
                    ))
                    .child(
                        div().absolute().right_0().bottom_0().child(
                            deferred(
                                anchored()
                                    .anchor(Corner::TopRight)
                                    .offset(point(px(0.), px(4.)))
                                    .snap_to_window_with_margin(Edges::all(px(8.)))
                                    .child(self.target_menu(target, cx)),
                            )
                            .with_priority(1),
                        ),
                    )
            });
        widgets::row(tr!("译成", "Translate into"), Some(description), palette)
            .when(off, |row| row.opacity(0.6))
            .child(control)
    }
    /// 自动置顶并写清规则；常用语言两列；末项“其它语言…”打开输入行。
    fn target_menu(&self, target: Target<'_>, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let menu_bg = if crate::theme::is_light(cx) {
            0xffffff
        } else {
            0x262d36
        };
        let check = |selected: bool| {
            div().size(px(14.)).flex_shrink_0().when(selected, |slot| {
                slot.child(toolbar_icons::action(Icon::Confirm, palette.accent).size(px(14.)))
            })
        };
        let cursor = self.translation_fields.menu_cursor;
        let item = |id: SharedString, index: usize, selected: bool| {
            div()
                .id(id.clone())
                .relative()
                .min_h(px(32.))
                .px(px(10.))
                .flex()
                .items_center()
                .gap_2()
                .rounded_md()
                .cursor_pointer()
                .text_size(px(13.))
                .when(cursor == Some(index), |item| item.bg(rgb(palette.hover)))
                .hover(move |style| style.bg(rgb(palette.hover)))
                .child(check(selected))
                .child(self.settings_probe(id))
        };
        let primary = move |text: SharedString, selected: bool| {
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .when(selected, |label| {
                    label
                        .text_color(rgb(palette.accent))
                        .font_weight(FontWeight::SEMIBOLD)
                })
                .child(text)
        };
        let secondary = move |text: &'static str| {
            div()
                .flex_shrink_0()
                .max_w(px(96.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(px(12.))
                .text_color(rgb(palette.muted))
                .child(text)
        };
        let separator = || div().h(px(1.)).mx(px(6.)).my_1().bg(rgb(surface.line));
        let english = crate::i18n::is_english();
        let auto = target == Target::Auto;
        let other = matches!(target, Target::Other(_));
        div()
            .id("settings-translation-target-menu")
            .track_focus(&self.translation_fields.menu_focus)
            .on_key_down(cx.listener(Self::target_menu_key))
            .occlude()
            .w(px(344.))
            .p_1()
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(rgb(palette.border))
            .bg(rgb(menu_bg))
            .shadow_lg()
            .child(
                item("settings-translation-target-auto".into(), 0, auto)
                    .min_h(px(44.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(primary(tr!("自动", "Auto").into(), auto))
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(palette.muted))
                                    .child(tr!(
                                        "中文译成英文，其它译成简体中文",
                                        "Chinese → English, others → Simplified Chinese"
                                    )),
                            ),
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.pick_target("auto", cx))),
            )
            .child(separator())
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .children(LANGUAGES.iter().enumerate().map(|(index, language)| {
                        let selected = target == Target::Known(language);
                        // 中文界面里中文变体的母语名已可读，不再重复副标题。
                        let subtitle = if english {
                            (language.en != language.native).then_some(language.en)
                        } else {
                            (!language.code.starts_with("zh")).then_some(language.zh)
                        };
                        let code = language.code;
                        item(
                            format!("settings-translation-target-{}", code.to_ascii_lowercase())
                                .into(),
                            index + 1,
                            selected,
                        )
                        .w(relative(0.5))
                        .child(primary(language.native.into(), selected))
                        .when_some(subtitle, |item, subtitle| item.child(secondary(subtitle)))
                        .on_click(cx.listener(move |view, _, _, cx| view.pick_target(code, cx)))
                    })),
            )
            .child(separator())
            .child(
                item(
                    "settings-translation-target-other".into(),
                    MENU_OTHER,
                    other,
                )
                .when(!other, |item| {
                    // 未选中时用编辑图标占住勾选位，提示这一项是输入。
                    item.child(
                        div()
                            .absolute()
                            .left(px(10.))
                            .top(px(9.))
                            .child(toolbar_icons::action(Icon::Edit, palette.muted).size(px(14.))),
                    )
                })
                .child(primary(
                    match target {
                        Target::Other(value) => value.to_owned().into(),
                        _ => tr!("其它语言…", "Other language…").into(),
                    },
                    other,
                ))
                .child(secondary(tr!("输入名称或代码", "Name or code")))
                .on_click(cx.listener(|view, _, window, cx| view.open_custom_target(window, cx))),
            )
    }
    fn custom_target_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px_3()
            .pb_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w(px(0.)).child(self.translation_field(
                        "settings-translation-target-input",
                        &self.translation_fields.target,
                    )))
                    .child(
                        widgets::button(
                            "settings-translation-target-save",
                            tr!("保存", "Save"),
                            ButtonKind::Primary,
                            palette,
                            surface,
                        )
                        .child(self.settings_probe("settings-translation-target-save"))
                        .on_click(cx.listener(|view, _, _, cx| view.save_custom_target(cx))),
                    ),
            )
            .child(widgets::note(
                tr!(
                    "回车保存；名称或代码会原样交给翻译模型。",
                    "Press Enter to save; the name or code is passed to the model as written."
                ),
                palette,
            ))
    }
    /// 高级里的折叠行：折叠时一行写清已保存的状态，点击展开参数。
    fn disclosure_row(
        &self,
        id: &'static str,
        title: &'static str,
        description: SharedString,
        badge: &'static str,
        highlight: bool,
        open: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        widgets::row(title, Some(description), palette)
            .id(id)
            .cursor_pointer()
            .hover(move |style| style.bg(rgb(surface.nav_hover)))
            .child(
                div()
                    .h(px(22.))
                    .px_2()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .rounded_md()
                    .text_size(px(12.))
                    .when(highlight, |badge| {
                        badge
                            .bg(rgba((palette.accent << 8) | 0x29))
                            .text_color(rgb(palette.accent))
                    })
                    .when(!highlight, |badge| {
                        badge.bg(rgb(surface.track)).text_color(rgb(palette.muted))
                    })
                    .child(badge),
            )
            .child(
                toolbar_icons::action(
                    if open { Icon::ChevronUp } else { Icon::Chevron },
                    palette.muted,
                )
                .size(px(16.)),
            )
            .child(self.settings_probe(id))
    }
    fn image_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (badge, description) = match self.preferences.image_translate_engine.trim() {
            "baidu" => (
                tr!("百度 V2", "Baidu V2"),
                tr!(
                    "上传选区到百度图片翻译 V2，返回擦除重绘的贴合图。",
                    "Uploads the selection to Baidu V2 and gets a re-rendered image."
                ),
            ),
            "local" => (
                tr!("本机排版", "Local layout"),
                tr!(
                    "本机识别后用上面的翻译服务排版，图片不出本机。",
                    "Local OCR plus the service above; the image stays on this PC."
                ),
            ),
            _ => (
                tr!("自动", "Auto"),
                tr!(
                    "本机识别后用上方服务翻译并排版，图片留在本机。",
                    "Local OCR, then translation with the service above; the image stays local."
                ),
            ),
        };
        self.disclosure_row(
            "settings-translation-image-row",
            tr!("图片翻译", "Image translation"),
            description.into(),
            badge,
            false,
            self.translation_fields.image_open,
            cx,
        )
        .on_click(cx.listener(|view, _, _, cx| {
            let fields = &mut view.translation_fields;
            fields.image_open = !fields.image_open;
            cx.notify();
        }))
    }
    fn image_panel(&self, cx: &mut Context<Self>) -> gpui::Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let engine = match self.translation_fields.engine.trim() {
            engine @ ("baidu" | "local") => engine,
            _ => "auto",
        };
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .pb(px(14.))
            .child(
                widgets::segmented(surface).children(
                    [
                        ("auto", tr!("自动", "Auto")),
                        ("baidu", tr!("百度 V2", "Baidu V2")),
                        ("local", tr!("本机排版", "Local layout")),
                    ]
                    .into_iter()
                    .map(|(value, label)| {
                        let id = format!("settings-translation-image-{value}");
                        widgets::segment(
                            SharedString::from(id.clone()),
                            label,
                            engine == value,
                            palette,
                            surface,
                        )
                        .child(self.settings_probe(id))
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.translation_fields.engine = value.to_owned();
                            view.status.clear();
                            cx.notify();
                        }))
                    }),
                ),
            )
            .when(engine == "baidu", |panel| {
                panel
                    .child(widgets::note(tr!("百度 APP ID", "Baidu APP ID"), palette))
                    .child(self.translation_field(
                        "settings-translation-baidu-app-id",
                        &self.translation_fields.baidu_app_id,
                    ))
                    .child(widgets::note("API Key", palette))
                    .child(self.translation_field(
                        "settings-translation-baidu-api-key",
                        &self.translation_fields.baidu_api_key,
                    ))
                    .child(widgets::note(
                        tr!(
                            "在百度翻译开放平台开通「图片翻译 V2.0」，APP ID 见管理控制台，API Key 在「API Key 管理」创建；只保存在本机。",
                            "Enable Picture Translation V2.0 on Baidu Translate Open Platform. The APP ID is in the console; create the API Key under API Key management. Stored locally only."
                        ),
                        palette,
                    ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(id: &str, preferences: &Preferences) -> ProfileDraft {
        preferences
            .service_profile(id)
            .map(ProfileDraft::from_profile)
            .unwrap_or_default()
    }

    #[test]
    fn builtin_profiles_keep_endpoint_and_only_deepseek_requires_a_key() {
        let latest = Preferences::default();
        let mut index = draft("index", &latest);
        index.endpoint = "https://stale.test/v1".into();
        index.vision = true;
        let (saved, id) = index.candidate(&latest).unwrap();
        let profile = saved.service_profile(&id).unwrap();
        assert_eq!(profile.endpoint, crate::translate::INDEX_ENDPOINT);
        assert!(!profile.supports_vision());
        assert_eq!(profile.effective_model(), crate::translate::INDEX_MODEL);
        let mut deepseek = draft("deepseek", &latest);
        assert!(deepseek.candidate(&latest).is_err());
        for invalid in ["unit test key", "key\nnext", "中文"] {
            deepseek.api_key = invalid.into();
            let error = deepseek.candidate(&latest).err().unwrap().to_string();
            assert!(!error.contains(invalid));
        }
        deepseek.api_key = "test-only-secret-value".into();
        assert!(!format!("{deepseek:?}").contains("test-only-secret-value"));
        let (saved, _) = deepseek.candidate(&latest).unwrap();
        assert_eq!(
            saved.service_profile("deepseek").unwrap().api_key,
            "test-only-secret-value"
        );
        assert_eq!(saved.translate_profile, latest.translate_profile);
    }

    #[test]
    fn new_custom_profile_validates_and_gets_a_fresh_id() {
        let latest = Preferences::default();
        let mut new = ProfileDraft::default();
        for endpoint in [
            "",
            "file:///C:/config",
            "https://example.test/v1?x=1",
            "https://example.test/v1#fragment",
            "http://",
            "https://example.test/v1\nhistory=true",
        ] {
            new.endpoint = endpoint.into();
            new.model = "m".into();
            assert!(new.candidate(&latest).is_err(), "{endpoint}");
        }
        new.endpoint = " http://127.0.0.1:11434/v1/ ".into();
        new.model.clear();
        assert!(new.candidate(&latest).is_err());
        new.model = " qwen3 ".into();
        new.vision = true;
        let (saved, id) = new.candidate(&latest).unwrap();
        assert_eq!(id, "custom-1");
        let profile = saved.service_profile(&id).unwrap();
        assert_eq!(profile.name, "127.0.0.1:11434/v1");
        assert_eq!(profile.endpoint, "http://127.0.0.1:11434/v1/");
        assert_eq!(profile.model, "qwen3");
        assert!(profile.supports_vision());
        assert!(!profile.needs_key());
        let (again, id) = ProfileDraft {
            name: "second".into(),
            ..new
        }
        .candidate(&saved)
        .unwrap();
        assert_eq!(id, "custom-2");
        assert_eq!(again.service_profiles.len(), 4);
    }

    #[test]
    fn stale_key_draft_keeps_newer_key_and_conflicting_edit_is_rejected() {
        let mut initial = Preferences::default();
        initial
            .service_profile_mut("deepseek")
            .unwrap()
            .set_api_key("old-test-key".into());
        let mut stale = draft("deepseek", &initial);
        let mut latest = initial.clone();
        latest
            .service_profile_mut("deepseek")
            .unwrap()
            .set_api_key("new-test-key".into());
        stale.model = "deepseek-pro".into();
        let (saved, _) = stale.candidate(&latest).unwrap();
        assert_eq!(
            saved.service_profile("deepseek").unwrap().api_key,
            "new-test-key"
        );
        stale.api_key = "edited-test-key".into();
        assert!(stale.candidate(&latest).is_err());
        let mut fresh = draft("deepseek", &latest);
        fresh.api_key = "edited-test-key".into();
        let (saved, _) = fresh.candidate(&latest).unwrap();
        assert_eq!(
            saved.service_profile("deepseek").unwrap().api_key,
            "edited-test-key"
        );
        let mut removed = Preferences::default();
        removed
            .service_profiles
            .retain(|profile| profile.id != "deepseek");
        assert!(draft("deepseek", &initial).candidate(&removed).is_err());
    }

    #[test]
    fn profile_save_changes_only_that_profile_and_other_pages_keep_latest_services() {
        let latest = Preferences {
            theme: crate::theme::ThemeMode::Dark,
            guides: true,
            translate_target: "ko".into(),
            formula_profile: String::new(),
            quick_dir: std::path::PathBuf::from("D:/saved-elsewhere"),
            ..Default::default()
        };
        let mut index = draft("index", &latest);
        index.model = "other".into();
        let (saved, _) = index.candidate(&latest).unwrap();
        assert_eq!(saved.theme, latest.theme);
        assert_eq!(saved.quick_dir, latest.quick_dir);
        assert_eq!(saved.translate_target, "ko");
        assert!(saved.formula_profile.is_empty());
        let mut stale_other_page = Preferences {
            include_cursor: true,
            ..Default::default()
        };
        preserve_latest(&mut stale_other_page, &saved);
        assert_eq!(stale_other_page.service_profiles, saved.service_profiles);
        assert!(stale_other_page.formula_profile.is_empty());
        assert_eq!(stale_other_page.translate_target, "ko");
        assert!(stale_other_page.include_cursor);
    }

    #[test]
    fn image_draft_only_reports_effective_changes() {
        let saved = Preferences::default();
        let mut image = ImageDraft::from_preferences(&saved);
        assert!(!image.changed(&saved));
        image.engine = "unknown".into();
        assert!(!image.changed(&saved));
        image.engine = "baidu".into();
        assert!(image.changed(&saved));
        image.baidu_app_id = "id\n".into();
        assert!(image.candidate(&saved).is_err());
        image.baidu_app_id = " id ".into();
        let candidate = image.candidate(&saved).unwrap();
        assert_eq!(candidate.image_translate_engine, "baidu");
        assert_eq!(candidate.baidu_app_id, "id");
    }

    #[test]
    fn formula_menu_only_offers_profiles_with_images() {
        let preferences = Preferences::default();
        let index = preferences.service_profile("index").unwrap();
        let deepseek = preferences.service_profile("deepseek").unwrap();
        assert!(Feature::Translate.accepts(index) && Feature::Translate.accepts(deepseek));
        assert!(!Feature::Formula.accepts(index) && Feature::Formula.accepts(deepseek));
    }

    #[test]
    fn target_menu_values_round_trip_and_custom_input_normalizes() {
        let ja = crate::translate::language("ja").unwrap();
        assert_eq!(Target::of(""), Target::Auto);
        assert_eq!(Target::of(" AUTO "), Target::Auto);
        assert_eq!(Target::of("ja"), Target::Known(ja));
        assert_eq!(
            Target::of("zh-hans"),
            Target::Known(crate::translate::language("zh-CN").unwrap())
        );
        assert_eq!(Target::of(" Svenska "), Target::Other("Svenska"));
        for input in ["日本語", " japanese ", "日语", "JA"] {
            assert_eq!(parse_target(input).as_deref(), Some("ja"), "{input}");
        }
        assert_eq!(parse_target("自动").as_deref(), Some("auto"));
        assert_eq!(parse_target(" Svenska\n").as_deref(), Some("Svenska"));
        assert_eq!(parse_target(" \t"), None);
        // 菜单项代码唯一，且都能被提示词识别成中文语言名。
        for (index, language) in LANGUAGES.iter().enumerate() {
            assert!(LANGUAGES[..index]
                .iter()
                .all(|other| other.code != language.code));
            assert_eq!(
                crate::translate::target_name(language.code, "x"),
                language.zh
            );
        }
    }

    #[test]
    fn menu_arrows_walk_auto_the_two_column_grid_and_other() {
        assert_eq!(LANGUAGES.len(), 14);
        assert_eq!(menu_step(None, "down"), Some(0));
        assert_eq!(menu_step(None, "up"), Some(MENU_OTHER));
        assert_eq!(menu_step(Some(0), "enter"), None);
        assert_eq!(menu_step(Some(0), "up"), Some(0));
        assert_eq!(menu_step(Some(0), "down"), Some(1));
        assert_eq!(menu_step(Some(1), "right"), Some(2));
        assert_eq!(menu_step(Some(2), "right"), Some(2));
        assert_eq!(menu_step(Some(2), "left"), Some(1));
        assert_eq!(menu_step(Some(2), "up"), Some(0));
        assert_eq!(menu_step(Some(3), "up"), Some(1));
        assert_eq!(menu_step(Some(12), "down"), Some(14));
        assert_eq!(menu_step(Some(13), "down"), Some(MENU_OTHER));
        assert_eq!(menu_step(Some(MENU_OTHER), "up"), Some(13));
        assert_eq!(menu_step(Some(MENU_OTHER), "down"), Some(MENU_OTHER));
    }
}
