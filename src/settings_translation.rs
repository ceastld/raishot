//! 翻译设置：首屏只有“译成”下拉，即选即存；翻译服务与图片翻译收进“高级”，
//! 改动先进草稿，与已保存值不同时才出现“应用”条。保存都不发送翻译请求。
use super::SettingsView;
use crate::{
    preferences::Preferences,
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

/// 文字翻译服务来源：内置只写默认端点与模型，自定义才使用两个输入框，关闭即清空端点。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextService {
    Builtin,
    Custom,
    Off,
}
impl TextService {
    fn detect(endpoint: &str, model: &str) -> Self {
        let (endpoint, model) = (endpoint.trim(), model.trim());
        if endpoint.is_empty() {
            Self::Off
        } else if endpoint.trim_end_matches('/') == crate::translate::DEFAULT_ENDPOINT
            && (model.is_empty() || model == crate::translate::DEFAULT_MODEL)
        {
            Self::Builtin
        } else {
            Self::Custom
        }
    }
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

/// “应用”保存的服务项；目标语言即选即存，不在草稿里。
fn service_key(preferences: &Preferences) -> [&str; 6] {
    [
        &preferences.translate_endpoint,
        &preferences.translate_model,
        &preferences.translate_api_key,
        &preferences.image_translate_engine,
        &preferences.baidu_app_id,
        &preferences.baidu_api_key,
    ]
}

#[derive(Clone, PartialEq, Eq)]
struct TranslationDraft {
    service: TextService,
    endpoint: String,
    model: String,
    api_key: String,
    /// 未编辑密钥的旧草稿不能覆盖另一窗口刚保存的新密钥。
    initial_api_key: String,
    engine: String,
    baidu_app_id: String,
    baidu_api_key: String,
}
impl std::fmt::Debug for TranslationDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TranslationDraft")
            .field("service", &self.service)
            .finish_non_exhaustive()
    }
}
impl TranslationDraft {
    fn from_preferences(preferences: &Preferences) -> Self {
        Self {
            service: TextService::detect(
                &preferences.translate_endpoint,
                &preferences.translate_model,
            ),
            endpoint: preferences.translate_endpoint.clone(),
            model: preferences.translate_model.clone(),
            api_key: preferences.translate_api_key.clone(),
            initial_api_key: preferences.translate_api_key.clone(),
            engine: preferences.image_translate_engine.clone(),
            baidu_app_id: preferences.baidu_app_id.clone(),
            baidu_api_key: preferences.baidu_api_key.clone(),
        }
    }
    fn candidate(&self, latest: &Preferences) -> Result<Preferences> {
        if [
            &self.endpoint,
            &self.model,
            &self.api_key,
            &self.baidu_app_id,
            &self.baidu_api_key,
        ]
        .iter()
        .any(|value| value.chars().any(char::is_control))
        {
            return Err(tr!(
                "翻译设置必须是单行文字，不能含控制字符。",
                "Translation settings must be single-line text without control characters."
            )
            .into());
        }
        let (endpoint, model) = match self.service {
            TextService::Builtin => (
                crate::translate::DEFAULT_ENDPOINT,
                crate::translate::DEFAULT_MODEL,
            ),
            TextService::Custom => (self.endpoint.trim(), self.model.trim()),
            TextService::Off => ("", self.model.trim()),
        };
        if self.service == TextService::Custom {
            if endpoint.is_empty() {
                return Err(tr!("请填写接口根地址。", "Enter the API base URL.").into());
            }
            // 沿现有 ureq 的公开 URL 解析；不调用 call/send，不解析 DNS 或连接服务。
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
        }
        let mut candidate = latest.clone();
        candidate.translate_endpoint = endpoint.to_owned();
        candidate.translate_model = model.to_owned();
        if self.api_key != self.initial_api_key {
            if latest.translate_api_key != self.initial_api_key
                && latest.translate_api_key != self.api_key.trim()
            {
                return Err(tr!(
                    "API Key 已在其它窗口更新，请取消修改读取最新设置后再编辑；本次草稿仍保留。",
                    "The API key changed in another window. Cancel changes to load the latest settings before editing; this draft is kept."
                )
                .into());
            }
            candidate.set_translate_api_key(self.api_key.trim().to_owned());
        }
        if !candidate.translate_api_key.is_ascii()
            || candidate.translate_api_key.chars().any(char::is_whitespace)
        {
            return Err(tr!(
                "API Key 不能包含空格、控制字符或非 ASCII 字符。",
                "The API key cannot contain spaces, control characters, or non-ASCII characters."
            )
            .into());
        }
        let needs_key = self.service == TextService::Builtin
            || ureq::request("POST", endpoint)
                .request_url()
                .is_ok_and(|url| url.as_url().host_str() == Some("api.deepseek.com"));
        if needs_key && candidate.translate_api_key.is_empty() {
            return Err(if candidate.translate_api_key_unreadable() {
                tr!(
                    "已保存的 API Key 无法解密，请重新填写 DeepSeek API Key。",
                    "The saved API key cannot be decrypted. Enter the DeepSeek API key again."
                )
            } else {
                tr!(
                    "请填写 DeepSeek API Key；保存不会发送请求。",
                    "Enter a DeepSeek API key. Saving does not send a request."
                )
            }
            .into());
        }
        candidate.image_translate_engine = match self.engine.trim() {
            engine @ ("baidu" | "local") => engine.to_owned(),
            _ => "auto".to_owned(),
        };
        candidate.baidu_app_id = self.baidu_app_id.trim().to_owned();
        candidate.baidu_api_key = self.baidu_api_key.trim().to_owned();
        Ok(candidate)
    }
    /// 与已保存值的实际效果不同（含校验不过的输入）时才显示“应用”条。
    fn changed(&self, saved: &Preferences) -> bool {
        let baseline = Self::from_preferences(saved);
        match (self.candidate(saved), baseline.candidate(saved)) {
            (Ok(draft), Ok(base)) => service_key(&draft) != service_key(&base),
            _ => *self != baseline,
        }
    }
}

/// 发布边界只调用现有 Preferences::save；验证或写入失败都不发布内存草稿。
fn publish(
    latest: &Preferences,
    draft: &TranslationDraft,
    save: impl FnOnce(&Preferences) -> Result<()>,
) -> Result<Preferences> {
    let candidate = draft.candidate(latest)?;
    save(&candidate)?;
    Ok(candidate)
}

/// 其它即时设置沿旧 save 路线时，不能把此窗口持有的旧翻译配置写回去。
pub(super) fn preserve_latest(preferences: &mut Preferences, latest: &Preferences) {
    preferences.translate_endpoint = latest.translate_endpoint.clone();
    preferences.translate_model = latest.translate_model.clone();
    preferences.preserve_translate_api_key_from(latest);
    preferences.translate_target = latest.translate_target.clone();
    preferences.image_translate_engine = latest.image_translate_engine.clone();
    preferences.baidu_app_id = latest.baidu_app_id.clone();
    preferences.baidu_api_key = latest.baidu_api_key.clone();
}

pub(super) struct TranslationFields {
    endpoint: Entity<TextField>,
    model: Entity<TextField>,
    api_key: Entity<TextField>,
    initial_api_key: String,
    /// “其它语言”输入框；回车直接保存目标语言，不进草稿。
    target: Entity<TextField>,
    baidu_app_id: Entity<TextField>,
    baidu_api_key: Entity<TextField>,
    /// 文字翻译服务来源草稿，随“应用”保存。
    service: TextService,
    /// 图片翻译引擎草稿（auto / baidu / local），随“应用”保存。
    engine: String,
    /// 目标语言下拉是否展开。
    target_menu: bool,
    /// 下拉打开时持有键盘焦点；高亮项见 `menu_step`。
    menu_focus: FocusHandle,
    menu_cursor: Option<usize>,
    /// “其它语言”输入行是否显示。
    custom_target: bool,
    /// 高级里“翻译服务”“图片翻译”两行是否展开。
    service_open: bool,
    image_open: bool,
    _subscriptions: Vec<Subscription>,
}
impl TranslationFields {
    pub(super) fn new(
        preferences: &Preferences,
        window: &mut Window,
        cx: &mut Context<SettingsView>,
    ) -> Self {
        let endpoint = cx.new(|cx| {
            TextField::new_settings(
                preferences.translate_endpoint.clone(),
                crate::i18n::Text("https://example.com/v1", "https://example.com/v1"),
                cx,
            )
        });
        let model = cx.new(|cx| {
            TextField::new_settings(
                preferences.translate_model.clone(),
                crate::i18n::Text(
                    crate::translate::DEFAULT_MODEL,
                    crate::translate::DEFAULT_MODEL,
                ),
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
                preferences.translate_api_key.clone(),
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
            &endpoint,
            &model,
            &api_key,
            &baidu_app_id,
            &baidu_api_key,
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
                    TextFieldEvent::Submit => view.apply_translation(cx),
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
            endpoint,
            model,
            api_key,
            initial_api_key: preferences.translate_api_key.clone(),
            target,
            baidu_app_id,
            baidu_api_key,
            service: TextService::detect(
                &preferences.translate_endpoint,
                &preferences.translate_model,
            ),
            engine: preferences.image_translate_engine.clone(),
            target_menu: false,
            menu_focus: cx.focus_handle(),
            menu_cursor: None,
            custom_target: false,
            service_open: preferences.translate_api_key_unreadable()
                || (preferences.translate_api_key.is_empty()
                    && TextService::detect(
                        &preferences.translate_endpoint,
                        &preferences.translate_model,
                    ) == TextService::Builtin),
            image_open: false,
            _subscriptions: subscriptions,
        }
    }
    fn read(&self, cx: &App) -> TranslationDraft {
        TranslationDraft {
            service: self.service,
            endpoint: self.endpoint.read(cx).text().to_owned(),
            model: self.model.read(cx).text().to_owned(),
            api_key: self.api_key.read(cx).text().to_owned(),
            initial_api_key: self.initial_api_key.clone(),
            engine: self.engine.clone(),
            baidu_app_id: self.baidu_app_id.read(cx).text().to_owned(),
            baidu_api_key: self.baidu_api_key.read(cx).text().to_owned(),
        }
    }
    pub(super) fn is_composing(&self, cx: &App) -> bool {
        [
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
    /// 只重置“应用”管的服务草稿；目标语言与展开状态不动。
    fn reset(&mut self, preferences: &Preferences, cx: &mut Context<SettingsView>) {
        let draft = TranslationDraft::from_preferences(preferences);
        self.service = draft.service;
        self.initial_api_key = preferences.translate_api_key.clone();
        self.engine = draft.engine;
        for (field, text) in [
            (&self.endpoint, draft.endpoint),
            (&self.model, draft.model),
            (&self.api_key, draft.api_key),
            (&self.baidu_app_id, draft.baidu_app_id),
            (&self.baidu_api_key, draft.baidu_api_key),
        ] {
            field.update(cx, |view, cx| view.set_text(text, cx));
        }
    }
    fn select_service(&mut self, service: TextService, cx: &mut Context<SettingsView>) {
        if service == TextService::Custom && self.service != TextService::Custom {
            let endpoint = self.endpoint.read(cx).text().trim().trim_end_matches('/');
            if endpoint.is_empty() || endpoint == crate::translate::DEFAULT_ENDPOINT {
                // 从内置／关闭切到自定义时清掉内置地址，避免误以为已填好自己的接口。
                for field in [&self.endpoint, &self.model] {
                    field.update(cx, |view, cx| view.set_text(String::new(), cx));
                }
            }
        }
        self.service = service;
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
    fn apply_translation(&mut self, cx: &mut Context<Self>) {
        if !self.translation_controls_ready(cx) {
            return;
        }
        let draft = self.translation_fields.read(cx);
        let latest = Preferences::load();
        self.status = match publish(&latest, &draft, Preferences::save) {
            Ok(saved) => {
                self.translation_fields.reset(&saved, cx);
                self.preferences = saved;
                if self.preferences.translate_endpoint.is_empty() {
                    tr!(
                        "已关闭文字翻译；设置已保存。",
                        "Text translation off; settings saved."
                    )
                    .into()
                } else {
                    tr!(
                        "服务设置已保存，下次翻译生效。",
                        "Service settings saved for the next translation."
                    )
                    .into()
                }
            }
            Err(error) => trf!(
                "翻译设置未保存：{error}",
                "Translation settings not saved: {error}"
            ),
        };
        cx.notify();
    }
    fn cancel_translation_changes(&mut self, cx: &mut Context<Self>) {
        if !self.translation_controls_ready(cx) {
            return;
        }
        let latest = Preferences::load();
        self.translation_fields.reset(&latest, cx);
        preserve_latest(&mut self.preferences, &latest);
        self.status = tr!(
            "已取消修改，保留已保存的翻译设置。",
            "Changes canceled; saved translation settings are kept."
        )
        .into();
        cx.notify();
    }
    /// 目标语言即时复位为自动；服务默认值只进草稿，仍需“应用”。
    fn reset_translation_defaults(&mut self, cx: &mut Context<Self>) {
        if !self.translation_controls_ready(cx) {
            return;
        }
        let mut defaults = Preferences::default();
        defaults.preserve_translate_api_key_from(&Preferences::load());
        self.translation_fields.reset(&defaults, cx);
        self.translation_fields.custom_target = false;
        if Target::of(&self.preferences.translate_target) != Target::Auto
            && !self.save_translation_target(defaults.translate_target.clone())
        {
            cx.notify();
            return;
        }
        self.status = if self.translation_fields.read(cx).changed(&self.preferences) {
            tr!(
                "“译成”已恢复为自动；服务默认值已填入，点击应用后生效。",
                "Target reset to Auto; service defaults filled in, click Apply to use them."
            )
        } else {
            tr!("已恢复默认。", "Defaults restored.")
        }
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
        let changed = fields.read(cx).changed(&self.preferences);
        let target_card = widgets::card(surface, [self.target_row(cx).into_any_element()])
            .when(fields.custom_target, |card| {
                card.child(self.custom_target_row(cx))
            });
        let service = div()
            .flex()
            .flex_col()
            .child(self.service_row(cx))
            .when(fields.service_open, |row| row.child(self.service_panel(cx)));
        let image = div()
            .flex()
            .flex_col()
            .child(self.image_row(cx))
            .when(fields.image_open, |row| row.child(self.image_panel(cx)));
        let advanced = widgets::section(tr!("高级", "Advanced"), palette).child(
            widgets::card(
                surface,
                [service.into_any_element(), image.into_any_element()],
            )
            .when(changed, |card| {
                card.child(
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
                            "服务设置有改动，应用后下次翻译生效。",
                            "Service changes take effect after Apply."
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
                            .on_click(
                                cx.listener(|view, _, _, cx| view.cancel_translation_changes(cx)),
                            ),
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
                            .on_click(cx.listener(|view, _, _, cx| view.apply_translation(cx))),
                        ),
                )
            }),
        );
        let footer = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(widgets::note(
                tr!(
                    "公式识别发送选区图片；翻译只发送文字；表格识别在本机。选择百度图片翻译时也会上传选区。",
                    "Formula recognition sends the selection image; translation sends text; tables stay local. Baidu image translation also uploads the selection."
                ),
                palette,
            ))
            .child(
                div()
                    .id("settings-translation-defaults")
                    .relative()
                    .px_1()
                    .cursor_pointer()
                    .text_size(px(12.))
                    .text_color(rgb(palette.accent))
                    .hover(|style| style.opacity(0.8))
                    .child(tr!("全部恢复默认", "Restore defaults"))
                    .child(self.settings_probe("settings-translation-defaults"))
                    .on_click(cx.listener(|view, _, _, cx| view.reset_translation_defaults(cx))),
            );
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_4()
            .child(target_card)
            .child(advanced)
            .child(footer)
            .into_any_element()
    }
    fn target_row(&self, cx: &mut Context<Self>) -> gpui::Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let off = self.preferences.translate_endpoint.trim().is_empty();
        let target = Target::of(&self.preferences.translate_target);
        let open = self.translation_fields.target_menu && !off;
        let description: SharedString = if off {
            tr!(
                "文字翻译已关闭，可在下方“翻译服务”中开启。",
                "Text translation is off. Turn it on under Translation service below."
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
    fn service_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let saved = &self.preferences;
        let service = TextService::detect(&saved.translate_endpoint, &saved.translate_model);
        let (badge, description): (_, SharedString) = match service {
            TextService::Builtin => (
                "DeepSeek",
                if saved.translate_api_key_unreadable() {
                    tr!(
                        "API Key 无法解密，请重新填写。",
                        "API key cannot be decrypted; enter it again."
                    )
                } else if saved.translate_api_key.is_empty() {
                    tr!(
                        "先填写 API Key，即可使用公式识别和翻译。",
                        "Enter an API key for formulas and translation."
                    )
                } else {
                    tr!(
                        "已配置 API Key，用于公式识别和翻译。",
                        "API key configured for formulas and translation."
                    )
                }
                .into(),
            ),
            TextService::Custom => {
                let model = match saved.translate_model.trim() {
                    "" => crate::translate::DEFAULT_MODEL,
                    model => model,
                };
                let host = saved.translate_endpoint.trim();
                let host = host
                    .strip_prefix("https://")
                    .or_else(|| host.strip_prefix("http://"))
                    .unwrap_or(host);
                (tr!("自定义", "Custom"), format!("{model} · {host}").into())
            }
            TextService::Off => (
                tr!("已关闭", "Off"),
                tr!(
                    "在线识别与翻译已关闭；本地表格识别仍可使用。",
                    "Online recognition and translation are off; local tables remain available."
                )
                .into(),
            ),
        };
        self.disclosure_row(
            "settings-translation-service-row",
            tr!("公式与翻译服务", "Formula and translation service"),
            description,
            badge,
            service == TextService::Builtin,
            self.translation_fields.service_open,
            cx,
        )
        .on_click(cx.listener(|view, _, _, cx| {
            let fields = &mut view.translation_fields;
            fields.service_open = !fields.service_open;
            cx.notify();
        }))
    }
    fn service_panel(&self, cx: &mut Context<Self>) -> gpui::Div {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let service = self.translation_fields.service;
        let panel = div().flex().flex_col().gap_2().px_3().pb(px(14.)).child(
            widgets::segmented(surface).children(
                [
                    (TextService::Builtin, "builtin", "DeepSeek"),
                    (TextService::Custom, "custom", tr!("自定义", "Custom")),
                    (TextService::Off, "off", tr!("关闭", "Off")),
                ]
                .into_iter()
                .map(|(value, key, label)| {
                    let id = format!("settings-translation-service-{key}");
                    widgets::segment(
                        SharedString::from(id.clone()),
                        label,
                        service == value,
                        palette,
                        surface,
                    )
                    .child(self.settings_probe(id))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.translation_fields.select_service(value, cx);
                        view.status.clear();
                        cx.notify();
                    }))
                }),
            ),
        );
        let panel = match service {
            TextService::Builtin => panel.child(builtin_model_card(palette, surface)),
            TextService::Custom => panel
                .child(widgets::note(tr!("接口根地址", "API base URL"), palette))
                .child(self.translation_field(
                    "settings-translation-endpoint",
                    &self.translation_fields.endpoint,
                ))
                .child(widgets::note(tr!("模型名称", "Model"), palette))
                .child(self.translation_field(
                    "settings-translation-model",
                    &self.translation_fields.model,
                ))
                .child(widgets::note(
                    tr!(
                        "OpenAI 兼容接口（如以 /v1 结尾）；模型留空使用默认模型。",
                        "OpenAI-compatible API (for example, ending in /v1). Leave the model empty for the default."
                    ),
                    palette,
                )),
            TextService::Off => panel.child(widgets::note(
                tr!(
                    "关闭在线公式识别与文字翻译；本地表格识别仍可使用。",
                    "Online formulas and text translation are off; local tables remain available."
                ),
                palette,
            )),
        };
        if service == TextService::Off {
            panel
        } else {
            panel
                .child(widgets::note(
                    if service == TextService::Builtin { "DeepSeek API Key" } else { tr!("API Key（可选）", "API key (optional)") },
                    palette,
                ))
                .child(self.translation_field("settings-translation-api-key", &self.translation_fields.api_key))
                .when(self.preferences.translate_api_key_unreadable(), |panel| {
                    panel.child(widgets::note(
                        tr!(
                            "原 API Key 无法解密，请重新填写；原密文仍保留。",
                            "The saved API key cannot be decrypted. Enter it again; its encrypted value is kept."
                        ),
                        palette,
                    ))
                })
                .child(widgets::note(
                    tr!(
                        "仅当前 Windows 用户可解密保存在本机的密钥；输入默认隐藏，支持粘贴。DeepSeek 需要 API Key，自定义本机服务可留空。",
                        "The saved key is encrypted for this Windows user. Input is hidden and can be pasted. DeepSeek requires a key; custom local services may leave it empty."
                    ),
                    palette,
                ))
        }
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

/// 内置服务只展示模型身份，不暴露端点和模型参数。
fn builtin_model_card(
    palette: crate::capture_toolbar::Palette,
    surface: widgets::Surface,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(rgb(surface.line))
        .bg(rgb(surface.track))
        .child(
            div()
                .size(px(36.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded_md()
                .bg(rgba((palette.accent << 8) | 0x2e))
                .child(toolbar_icons::action(Icon::Translate, palette.accent).size(px(20.))),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(palette.foreground))
                        .child(tr!(
                            "DeepSeek 公式识别与翻译",
                            "DeepSeek formulas and translation"
                        ))
                        .child(widgets::badge(tr!("自备密钥", "Your API key"), palette, surface)),
                )
                .child(widgets::note(
                    tr!(
                        "使用 DeepSeek API；公式只发送选区图，翻译只发送文字，表格识别在本机。",
                        "DeepSeek API: selection images for formulas, text for translation; tables stay local."
                    ),
                    palette,
                )),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn api_key_missing_or_invalid_draft_does_not_write_or_echo_secrets() {
        let latest = Preferences::default();
        let mut draft = TranslationDraft::from_preferences(&latest);
        let writes = Cell::new(0);
        assert!(publish(&latest, &draft, |_| {
            writes.set(writes.get() + 1);
            Ok(())
        })
        .is_err());
        assert_eq!(writes.get(), 0);
        for invalid in ["unit test key", "key\nnext", "中文"] {
            draft.api_key = invalid.into();
            let error = draft.candidate(&latest).err().unwrap().to_string();
            assert!(!error.contains(invalid));
        }
        draft.api_key = "test-only-secret-value".into();
        assert!(!format!("{draft:?}").contains("test-only-secret-value"));
        let saved = publish(&latest, &draft, |_| Ok(())).unwrap();
        assert_eq!(saved.translate_api_key, "test-only-secret-value");
    }

    #[test]
    fn api_key_stale_draft_preserves_newer_key_and_conflicting_edit_is_rejected() {
        let initial = Preferences {
            translate_api_key: "old-test-key".into(),
            ..Default::default()
        };
        let mut draft = TranslationDraft::from_preferences(&initial);
        let latest = Preferences {
            translate_api_key: "new-test-key".into(),
            ..initial.clone()
        };
        draft.engine = "local".into();
        let saved = publish(&latest, &draft, |_| Ok(())).unwrap();
        assert_eq!(saved.translate_api_key, "new-test-key");
        draft.api_key = "edited-test-key".into();
        let writes = Cell::new(0);
        assert!(publish(&latest, &draft, |_| {
            writes.set(writes.get() + 1);
            Ok(())
        })
        .is_err());
        assert_eq!(writes.get(), 0);
        let mut draft = TranslationDraft::from_preferences(&latest);
        draft.api_key = "edited-test-key".into();
        assert!(publish(&latest, &draft, |_| Err("模拟写入失败".into())).is_err());
        assert_eq!(latest.translate_api_key, "new-test-key");
        assert_eq!(draft.api_key, "edited-test-key");
        assert_eq!(
            publish(&latest, &draft, |_| Ok(()))
                .unwrap()
                .translate_api_key,
            "edited-test-key"
        );
        let mut stale_other_page = initial;
        stale_other_page.guides = true;
        preserve_latest(&mut stale_other_page, &latest);
        assert_eq!(stale_other_page.translate_api_key, "new-test-key");
        assert!(stale_other_page.guides);
    }

    #[test]
    fn builtin_service_hides_parameters_and_saves_default_endpoint_and_model() {
        let latest = Preferences {
            translate_api_key: "unit-test-key".into(),
            ..Default::default()
        };
        let mut draft = TranslationDraft::from_preferences(&latest);
        assert_eq!(draft.service, TextService::Builtin);
        draft.endpoint = "https://stale.test/v1".into();
        draft.model = "stale".into();
        let saved = draft.candidate(&latest).unwrap();
        assert_eq!(saved.translate_endpoint, crate::translate::DEFAULT_ENDPOINT);
        assert_eq!(saved.translate_model, crate::translate::DEFAULT_MODEL);
        assert_eq!(
            TextService::detect(&format!("{}/", crate::translate::DEFAULT_ENDPOINT), ""),
            TextService::Builtin
        );
        assert_eq!(
            TextService::detect(crate::translate::DEFAULT_ENDPOINT, "other"),
            TextService::Custom
        );
        assert_eq!(TextService::detect(" ", "x"), TextService::Off);
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

    #[test]
    fn apply_bar_only_shows_for_effective_service_changes() {
        let saved = Preferences {
            translate_model: String::new(),
            translate_target: "ja".into(),
            translate_api_key: "unit-test-key".into(),
            ..Default::default()
        };
        let mut draft = TranslationDraft::from_preferences(&saved);
        // 内置且模型留空与默认模型等价，不算改动；目标语言不在草稿里。
        assert!(!draft.changed(&saved));
        draft.service = TextService::Custom;
        draft.endpoint.clear();
        assert!(draft.changed(&saved));
        assert!(draft.candidate(&saved).is_err());
        draft.service = TextService::Builtin;
        assert!(!draft.changed(&saved));
        draft.engine = "local".into();
        assert!(draft.changed(&saved));
        assert_eq!(draft.candidate(&saved).unwrap().translate_target, "ja");
    }

    #[test]
    fn draft_changes_only_translation_fields_of_the_latest_preferences() {
        let latest = Preferences {
            theme: crate::theme::ThemeMode::Dark,
            language: crate::i18n::LanguageMode::English,
            ocr_profile: crate::ppocr::OcrProfile::Small,
            history: true,
            include_cursor: true,
            pin_restore: false,
            guides: true,
            quick_dir: std::path::PathBuf::from("D:/saved-elsewhere"),
            translate_target: "ko".into(),
            ..Default::default()
        };
        let draft = TranslationDraft {
            service: TextService::Custom,
            endpoint: " http://127.0.0.1:8000/v1/ ".into(),
            model: " local-model ".into(),
            ..TranslationDraft::from_preferences(&Preferences::default())
        };
        let saved = publish(&latest, &draft, |_| Ok(())).unwrap();
        assert_eq!(saved.translate_endpoint, "http://127.0.0.1:8000/v1/");
        assert_eq!(saved.translate_model, "local-model");
        assert_eq!(saved.translate_target, "ko");
        assert_eq!(saved.theme, latest.theme);
        assert_eq!(saved.language, latest.language);
        assert_eq!(saved.ocr_profile, latest.ocr_profile);
        assert_eq!(saved.history, latest.history);
        assert_eq!(saved.guides, latest.guides);
        assert_eq!(saved.pin_restore, latest.pin_restore);
        assert_eq!(saved.include_cursor, latest.include_cursor);
        assert_eq!(saved.quick_dir, latest.quick_dir);
        assert_eq!(saved.custom_hotkey, latest.custom_hotkey);
        assert_eq!(saved.custom_pin_hotkey, latest.custom_pin_hotkey);
        assert_eq!(saved.pin_zoom_anchor, latest.pin_zoom_anchor);
    }

    #[test]
    fn defaults_off_and_custom_default_model_keep_existing_service_semantics() {
        let latest = Preferences {
            translate_api_key: "unit-test-key".into(),
            ..Default::default()
        };
        let defaults = TranslationDraft::from_preferences(&latest);
        assert_eq!(defaults.endpoint, crate::translate::DEFAULT_ENDPOINT);
        assert_eq!(defaults.model, crate::translate::DEFAULT_MODEL);
        assert_eq!(latest.translate_target, "auto");
        assert_eq!(defaults.service, TextService::Builtin);
        let mut draft = defaults;
        draft.service = TextService::Off;
        let disabled = draft.candidate(&latest).unwrap();
        assert!(crate::translate::Service::from_preferences(&disabled).is_none());
        draft.service = TextService::Custom;
        draft.endpoint = crate::translate::DEFAULT_ENDPOINT.into();
        draft.model.clear();
        let service =
            crate::translate::Service::from_preferences(&draft.candidate(&latest).unwrap())
                .unwrap();
        assert_eq!(service.model, crate::translate::DEFAULT_MODEL);
        assert_eq!(
            crate::translate::target_name(&service.target, "中文"),
            "英语"
        );
        let french = Preferences {
            translate_target: "法语".into(),
            translate_api_key: "unit-test-key".into(),
            ..Default::default()
        };
        let service =
            crate::translate::Service::from_preferences(&draft.candidate(&french).unwrap())
                .unwrap();
        assert_eq!(
            crate::translate::target_name(&service.target, "source"),
            "法语"
        );
    }

    #[test]
    fn validation_and_write_failure_keep_the_draft_and_only_success_publishes() {
        let latest = Preferences::default();
        let before = TranslationDraft::from_preferences(&latest);
        let mut draft = before.clone();
        draft.service = TextService::Custom;
        let writes = Cell::new(0);
        for endpoint in [
            "",
            "file:///C:/config",
            "https://example.test/v1?x=1",
            "https://example.test/v1#fragment",
            "http://",
            "https://example.test/v1\nhistory=true",
        ] {
            draft.endpoint = endpoint.into();
            assert!(publish(&latest, &draft, |_| {
                writes.set(writes.get() + 1);
                Ok(())
            })
            .is_err());
        }
        assert_eq!(writes.get(), 0);
        draft.endpoint = "https://example.test/v1".into();
        assert!(publish(&latest, &draft, |_| {
            writes.set(writes.get() + 1);
            Err("模拟原子保存失败".into())
        })
        .is_err());
        assert_eq!(writes.get(), 1);
        assert_eq!(TranslationDraft::from_preferences(&latest), before);
        assert_eq!(draft.endpoint, "https://example.test/v1");
        let saved = publish(&latest, &draft, |_| {
            writes.set(writes.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(writes.get(), 2);
        assert_eq!(saved.translate_endpoint, "https://example.test/v1");
    }

    #[test]
    fn other_setting_save_preserves_newer_translation_and_its_own_pending_change() {
        let mut stale = Preferences {
            guides: true,
            ..Default::default()
        };
        let latest = Preferences {
            translate_endpoint: String::new(),
            translate_model: "custom".into(),
            translate_target: "ko".into(),
            ..Default::default()
        };
        preserve_latest(&mut stale, &latest);
        assert_eq!(
            TranslationDraft::from_preferences(&stale),
            TranslationDraft::from_preferences(&latest)
        );
        assert_eq!(stale.translate_target, "ko");
        assert!(stale.guides);
    }
}
