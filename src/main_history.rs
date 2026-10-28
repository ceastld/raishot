//! 主界面图库页：截图、贴图与录屏的统一缩略图时间线。
//! 模型与动作在 `library`；这里只管筛选、布局、选择、详情和可撤销删除，工具层在 `tools`。
#[path = "main_history_recovery.rs"]
mod recovery;
#[path = "main_history_tools.rs"]
mod tools;

use super::{widgets, SettingsView};
use crate::{
    library::{self, Filter, Item, PinState, Source},
    preferences::Preferences,
    recording_library_model::{self as recordings, OpenAction},
    settings_widgets::ButtonKind,
    text_field::{TextField, TextFieldEvent},
    toolbar_icons::Icon,
    tray_request::{self, Request},
    video_view::HistoryVideoPreview,
};
use gpui::{
    div, img, prelude::*, px, rgb, rgba, AnyElement, ClickEvent, Context, Entity, FocusHandle,
    FontWeight, KeyDownEvent, ObjectFit, RenderImage, ScrollHandle, SharedString, Subscription,
    Window,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime},
};

/// 删除后可撤销的时间；到期才真正删除。
const UNDO_SECONDS: u64 = 10;
/// 窗口重新激活时，距上次扫描超过这么久才重新扫描。
const STALE_SECONDS: u64 = 10;
const DETAIL_WIDTH: f32 = 300.;
const ROW_HEIGHT: f32 = 140.;
const GAP: f32 = 8.;

pub(crate) struct HistoryPage {
    items: Vec<Item>,
    warnings: Vec<String>,
    loading: bool,
    scanned_at: Option<Instant>,
    generation: Arc<AtomicU64>,
    /// 缩略图加载代次；历史页与贴图页共用缓存，后发起的加载取代先前的。
    thumb_generation: Arc<AtomicU64>,
    filter: Filter,
    search: Entity<TextField>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    /// 当前项：详情栏、方向键与 Shift 范围的锚点。
    selected: Option<String>,
    /// 按选择顺序的选区；单选时只含当前项。
    chosen: Vec<String>,
    tools: tools::ToolState,
    thumbs: HashMap<String, Arc<RenderImage>>,
    thumb_failed: HashSet<String>,
    thumb_revisions: HashMap<String, library::ThumbnailRevision>,
    thumb_pending: HashMap<String, library::ThumbnailRevision>,
    /// 已从列表隐藏、等待撤销期满的删除。
    pending: Vec<Item>,
    pending_serial: u64,
    detail: bool,
    /// 上一帧的布局：每个条目所在的滚动子元素序号与首行条目数，供方向键使用。
    child_index: HashMap<String, usize>,
    row_hint: usize,
    recovery: recovery::RecoveryPanel,
    video_preview: HistoryVideoPreview,
    _subscriptions: Vec<Subscription>,
}
impl HistoryPage {
    pub(super) fn new(window: &mut Window, cx: &mut Context<SettingsView>) -> Self {
        let search = cx.new(|cx| {
            TextField::new(
                String::new(),
                crate::tr!("搜索标题、日期、尺寸", "Search title, date, size"),
                cx,
            )
        });
        search.update(cx, |field, cx| {
            field.configure_focus(vec![search.downgrade()], window, cx)
        });
        let subscription = cx.subscribe_in(
            &search,
            window,
            |view: &mut SettingsView, _, event, _, cx| {
                if matches!(event, TextFieldEvent::Changed) {
                    view.history_preview_tick(true, cx);
                    cx.notify();
                }
            },
        );
        Self {
            items: Vec::new(),
            warnings: Vec::new(),
            loading: false,
            scanned_at: None,
            generation: Arc::new(AtomicU64::new(0)),
            thumb_generation: Arc::new(AtomicU64::new(0)),
            filter: Filter::All,
            search,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            selected: None,
            chosen: Vec::new(),
            tools: tools::ToolState::default(),
            thumbs: HashMap::new(),
            thumb_failed: HashSet::new(),
            thumb_revisions: HashMap::new(),
            thumb_pending: HashMap::new(),
            pending: Vec::new(),
            pending_serial: 0,
            detail: true,
            child_index: HashMap::new(),
            row_hint: 4,
            recovery: recovery::RecoveryPanel::default(),
            video_preview: HistoryVideoPreview::default(),
            _subscriptions: vec![subscription],
        }
    }

    pub(super) fn hide_recovery(&mut self) {
        self.recovery.hide();
    }
}
impl Drop for HistoryPage {
    /// 关窗时撤销期未满的删除立即生效，与提示一致。
    fn drop(&mut self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.thumb_generation.fetch_add(1, Ordering::AcqRel);
        for item in self.pending.drain(..) {
            let _ = library::delete(&item);
        }
    }
}

fn aspect(item: &Item) -> f32 {
    match (item.size, &item.source) {
        (Some((w, h)), _) if w > 0 && h > 0 => w as f32 / h as f32,
        (_, Source::Recording(_)) => 16. / 9.,
        _ => 4. / 3.,
    }
}

impl SettingsView {
    /// 宿主切页或隐藏窗口时先停止预览；这里不会创建或恢复播放会话。
    pub(super) fn history_preview_tick(&mut self, visible: bool, cx: &mut Context<Self>) {
        if !visible
            || self.media_popup.is_open()
            || self.page != super::MainPage::History
            || !self.history.detail
            || self.history.recovery.visible
        {
            self.history_clear_video_preview(cx);
            return;
        }
        let selected_visible = !self.history_multi()
            && self.history.tools.pick.is_none()
            && self.history_visible(cx).into_iter().any(|index| {
                let item = &self.history.items[index];
                self.history.selected.as_deref() == Some(item.key.as_str())
                    && matches!(item.source, Source::Recording(_))
            });
        if !selected_visible {
            self.history_clear_video_preview(cx);
        }
    }

    pub(super) fn history_clear_video_preview(&mut self, cx: &mut Context<Self>) {
        self.history.video_preview.clear(cx);
    }

    pub(super) fn history_popup_return_focus(
        &self,
        window: &Window,
        cx: &gpui::App,
    ) -> Option<FocusHandle> {
        if self.history.video_preview.has_focus(window, cx) {
            Some(self.history.focus.clone())
        } else {
            window.focused(cx)
        }
    }

    pub(super) fn history_refresh_if_stale(&mut self, cx: &mut Context<Self>) {
        let stale = self
            .history
            .scanned_at
            .is_none_or(|at| at.elapsed() >= Duration::from_secs(STALE_SECONDS));
        if !self.history.loading && stale {
            self.history_scan(cx);
        } else if !self.history.loading {
            // 贴图页共用缩略图代次；返回时接续被切页取消的历史队列。
            self.history_load_thumbs(cx);
        }
    }

    fn history_refresh(&mut self, cx: &mut Context<Self>) {
        self.history.thumb_generation.fetch_add(1, Ordering::AcqRel);
        self.history.thumbs.clear();
        self.history.thumb_failed.clear();
        self.history.thumb_revisions.clear();
        self.history.thumb_pending.clear();
        self.history_scan(cx);
    }

    fn history_scan(&mut self, cx: &mut Context<Self>) {
        let generation = self.history.generation.clone();
        let token = generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.history.loading = true;
        let task = cx
            .background_executor()
            .spawn(async move { library::scan(&|| generation.load(Ordering::Acquire) != token) });
        cx.spawn(async move |this, cx| {
            let scan = task.await;
            let _ = this.update(cx, |view, cx| view.history_scanned(token, scan, cx));
        })
        .detach();
        cx.notify();
    }

    fn history_scanned(&mut self, token: u64, scan: library::Scan, cx: &mut Context<Self>) {
        if self.history.generation.load(Ordering::Acquire) != token {
            return;
        }
        let page = &mut self.history;
        page.loading = false;
        page.scanned_at = Some(Instant::now());
        page.items = scan.items;
        page.warnings = scan.warnings;
        let keys: HashSet<&str> = page.items.iter().map(|item| item.key.as_str()).collect();
        page.thumbs.retain(|key, _| keys.contains(key.as_str()));
        page.thumb_failed.retain(|key| keys.contains(key.as_str()));
        page.thumb_revisions
            .retain(|key, _| keys.contains(key.as_str()));
        page.chosen.retain(|key| keys.contains(key.as_str()));
        if page
            .selected
            .as_deref()
            .is_some_and(|key| !keys.contains(key))
        {
            page.selected = None;
            page.video_preview.clear(cx);
        }
        self.history_load_thumbs(cx);
        cx.notify();
    }

    /// 按时间线顺序逐张解码，最新的先出现；扫描换代即停止。
    fn history_load_thumbs(&mut self, cx: &mut Context<Self>) {
        let jobs = self
            .history
            .items
            .iter()
            .map(|item| (item.key.clone(), item.source.clone()))
            .collect();
        self.ensure_thumbs(jobs, cx);
    }

    pub(super) fn history_show_pins(&mut self) {
        self.history.filter = Filter::Pins;
    }

    pub(super) fn history_thumb(&self, key: &str) -> Option<Arc<RenderImage>> {
        self.history.thumbs.get(key).cloned()
    }

    /// 按给定顺序补齐缩略图；文字内容直接展示，不栅格化为底图。
    pub(super) fn ensure_thumbs(&mut self, jobs: Vec<(String, Source)>, cx: &mut Context<Self>) {
        let mut pending = Vec::new();
        let mut revisions = HashMap::new();
        for (key, source) in jobs {
            let revision = library::thumbnail_revision(&source);
            if self.history.thumb_revisions.get(&key) != Some(&revision) {
                self.history.thumbs.remove(&key);
                self.history.thumb_failed.remove(&key);
                self.history
                    .thumb_revisions
                    .insert(key.clone(), revision.clone());
            }
            if !self.history.thumbs.contains_key(&key)
                && !self.history.thumb_failed.contains(&key)
                && !matches!(source, Source::Pin { text: true, .. })
            {
                revisions.insert(key.clone(), revision);
                pending.push((key, source));
            }
        }
        // 贴图页定期重读清单；相同在途任务继续执行，避免慢图每次都从头解码。
        if revisions == self.history.thumb_pending {
            return;
        }
        let generation = self.history.thumb_generation.clone();
        let token = generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.history.thumb_pending = revisions;
        if pending.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            for (key, source) in pending {
                if generation.load(Ordering::Acquire) != token {
                    break;
                }
                let cancel = generation.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        library::thumbnail(&source, &|| cancel.load(Ordering::Acquire) != token)
                            .map_err(|error| error.to_string())
                    })
                    .await;
                if generation.load(Ordering::Acquire) != token {
                    break;
                }
                let image = match result {
                    Ok(Some(thumb)) => {
                        image::RgbaImage::from_raw(thumb.width, thumb.height, thumb.bgra)
                            // GPUI RenderImage 的像素合同是 BGRA。
                            .map(|pixels| {
                                Arc::new(RenderImage::new(vec![image::Frame::new(pixels)]))
                            })
                    }
                    _ => None,
                };
                let updated = this.update(cx, |view, cx| {
                    if view.history.thumb_generation.load(Ordering::Acquire) != token {
                        return;
                    }
                    view.history.thumb_pending.remove(&key);
                    match image {
                        Some(image) => {
                            view.history.thumbs.insert(key, image);
                        }
                        None => {
                            view.history.thumb_failed.insert(key);
                        }
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn history_visible(&self, cx: &gpui::App) -> Vec<usize> {
        let query = self.history.search.read(cx).text().to_owned();
        let today = library::local_time(SystemTime::now());
        self.history
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                self.history.filter.accepts(item)
                    && !self.history.pending.iter().any(|p| p.key == item.key)
                    && library::matches(item, &query, today.as_ref())
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn history_selected(&self) -> Option<&Item> {
        let key = self.history.selected.as_deref()?;
        self.history.items.iter().find(|item| item.key == key)
    }

    fn history_select(&mut self, key: String, cx: &mut Context<Self>) {
        if let Some(&child) = self.history.child_index.get(&key) {
            self.history.scroll.scroll_to_item(child);
        }
        self.history_set_single(Some(key), cx);
        cx.notify();
    }

    fn history_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let visible = self.history_visible(cx);
        if visible.is_empty() {
            return;
        }
        let current = self.history.selected.as_deref().and_then(|key| {
            visible
                .iter()
                .position(|&index| self.history.items[index].key == key)
        });
        let next = match current {
            None => 0,
            Some(position) => position.saturating_add_signed(delta).min(visible.len() - 1),
        };
        let key = self.history.items[visible[next]].key.clone();
        self.history_select(key, cx);
    }

    /// 双击／Enter：截图贴出，关闭的贴图恢复，录屏内置播放。
    fn history_primary(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.history_selected().cloned() else {
            return;
        };
        let result: Result<&str, String> = match &item.source {
            Source::Shot(entry) => tray_request::send(Request::PinFile(entry.path.clone()))
                .map(|_| crate::tr!("已贴出", "Pinned"))
                .map_err(|e| e.to_string()),
            Source::Pin {
                state: PinState::Live,
                ..
            } => Ok(crate::tr!(
                "这张贴图已在屏幕上；在“贴图”页管理",
                "This pin is already on screen; manage it on the Pins page"
            )),
            Source::Pin { id, .. } => {
                let sent = tray_request::send(Request::PinRecord(id.clone()))
                    .map(|_| crate::tr!("已恢复贴图", "Pin restored"))
                    .map_err(|e| e.to_string());
                if sent.is_ok() {
                    self.history_rescan_later(cx);
                }
                sent
            }
            Source::Recording(entry) => {
                self.open_media_popup(entry.path.clone(), true, cx);
                return;
            }
        };
        self.status = match result {
            Ok(message) => message.into(),
            Err(error) => crate::trf!("操作失败：{error}", "Action failed: {error}"),
        };
        cx.notify();
    }

    /// 贴图宿主恢复需要一点时间；稍后重扫一次以更新“屏幕上”状态。
    fn history_rescan_later(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(2)).await;
            let _ = this.update(cx, |view, cx| view.history_scan(cx));
        })
        .detach();
    }

    fn history_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.history_selected().cloned() else {
            return;
        };
        let owner = crate::gpui_capture::hwnd(window);
        match item.source {
            Source::Shot(entry) => {
                let owner = owner.0 as isize;
                let task = cx.background_executor().spawn(async move {
                    crate::history::open_image(&entry, &|| false).map_err(|e| e.to_string())
                });
                cx.spawn(async move |this, cx| {
                    let result = task.await.and_then(|image| {
                        crate::export::copy_image(
                            &image,
                            windows::Win32::Foundation::HWND(owner as *mut _),
                        )
                        .map_err(|e| e.to_string())
                    });
                    let _ = this.update(cx, |view, cx| {
                        view.status = match result {
                            Ok(()) => crate::tr!("已复制图片", "Image copied").into(),
                            Err(error) => {
                                crate::trf!("复制失败：{error}", "Copy failed: {error}")
                            }
                        };
                        cx.notify();
                    });
                })
                .detach();
            }
            Source::Recording(entry) => {
                self.status =
                    match crate::recording_view::copy_file_to_clipboard(owner, &entry.path) {
                        Ok(()) => crate::tr!("已复制录屏文件", "Recording file copied").into(),
                        Err(error) => crate::trf!("复制失败：{error}", "Copy failed: {error}"),
                    };
                cx.notify();
            }
            Source::Pin { .. } => {}
        }
    }

    fn history_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(Source::Shot(entry)) = self.history_selected().map(|item| &item.source) {
            if let Err(error) = tray_request::send(Request::EditFile(entry.path.clone())) {
                self.status = crate::trf!("无法打开编辑：{error}", "Cannot open editor: {error}");
                cx.notify();
            }
        }
    }

    fn history_reveal(&mut self, cx: &mut Context<Self>) {
        let result = match self.history_selected().map(|item| &item.source) {
            Some(Source::Shot(entry)) => {
                library::reveal(&entry.path).map_err(|error| error.to_string())
            }
            Some(Source::Recording(entry)) => recordings::open(&entry.path, OpenAction::Locate),
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.status = crate::trf!("无法定位文件：{error}", "Cannot show the file: {error}");
            cx.notify();
        }
    }

    /// 先从列表隐藏并给出撤销；期满后在后台真正删除。
    fn history_delete(&mut self, cx: &mut Context<Self>) {
        if self.history_multi() {
            let items: Vec<Item> = self
                .history_chosen()
                .into_iter()
                .filter(|item| item.can_delete())
                .cloned()
                .collect();
            if items.is_empty() {
                self.status = crate::tr!(
                    "所选贴图仍在屏幕上，请先关闭",
                    "The selected pins are still on screen; close them first"
                )
                .into();
                cx.notify();
                return;
            }
            self.history_set_single(None, cx);
            self.history_defer_delete(items, cx);
            return;
        }
        let Some(item) = self.history_selected().cloned() else {
            return;
        };
        if !item.can_delete() {
            self.status = crate::tr!(
                "贴图仍在屏幕上，请先关闭",
                "The pin is still on screen; close it first"
            )
            .into();
            cx.notify();
            return;
        }
        // 选中顺延到下一项，连续按 Delete 可逐个删除。
        self.history_clear_video_preview(cx);
        let visible = self.history_visible(cx);
        let position = visible
            .iter()
            .position(|&index| self.history.items[index].key == item.key);
        let next = position.and_then(|position| {
            visible
                .get(position + 1)
                .or_else(|| position.checked_sub(1).and_then(|p| visible.get(p)))
                .map(|&index| self.history.items[index].key.clone())
        });
        self.history_set_single(next, cx);
        self.history_defer_delete(vec![item], cx);
    }

    fn history_defer_delete(&mut self, items: Vec<Item>, cx: &mut Context<Self>) {
        self.history.pending.extend(items);
        self.history.pending_serial += 1;
        let serial = self.history.pending_serial;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(UNDO_SECONDS))
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.history.pending_serial == serial {
                    view.history_commit_deletes(cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn history_undo(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.history.pending.last().map(|item| item.key.clone()) {
            self.history_set_single(Some(key), cx);
        }
        self.history.pending.clear();
        self.history.pending_serial += 1;
        cx.notify();
    }

    fn history_commit_deletes(&mut self, cx: &mut Context<Self>) {
        let pending = std::mem::take(&mut self.history.pending);
        if pending.is_empty() {
            return;
        }
        let task = cx.background_executor().spawn(async move {
            pending
                .iter()
                .filter_map(|item| library::delete(item).err().map(|e| e.to_string()))
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let errors = task.await;
            let _ = this.update(cx, |view, cx| {
                if let Some(error) = errors.first() {
                    view.status = crate::trf!(
                        "{} 项删除失败：{error}",
                        "{} items could not be deleted: {error}",
                        errors.len()
                    );
                }
                view.history_scan(cx);
            });
        })
        .detach();
    }

    fn history_enable_recording(&mut self, cx: &mut Context<Self>) {
        let previous = Preferences::load();
        self.preferences = previous.clone();
        self.preferences.history = true;
        if !self.save() {
            self.preferences = previous;
        }
        cx.notify();
    }

    fn history_view_internal(&mut self, cx: &mut Context<Self>) {
        let Some(Source::Shot(entry)) = self.history_selected().map(|item| &item.source) else {
            return;
        };
        self.open_media_popup(entry.path.clone(), false, cx);
    }

    fn history_play_internal(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.history_selected() else {
            return;
        };
        let Source::Recording(entry) = &item.source else {
            return;
        };
        self.open_media_popup(entry.path.clone(), true, cx);
    }

    fn history_open_system(&mut self, cx: &mut Context<Self>) {
        let Some(Source::Recording(entry)) = self.history_selected().map(|item| &item.source)
        else {
            return;
        };
        self.status = match recordings::open(&entry.path, OpenAction::Open) {
            Ok(()) => crate::tr!("已用系统应用打开", "Opened in the system app").into(),
            Err(error) => crate::trf!("无法打开：{error}", "Cannot open: {error}"),
        };
        cx.notify();
    }

    fn history_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.media_popup.is_open() {
            return;
        }
        let search = self.history.search.read(cx);
        let searching = search.focus_handle.is_focused(window);
        if search.is_composing() {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        let plain = !modifiers.control && !modifiers.alt && !modifiers.shift && !modifiers.platform;
        let key = event.keystroke.key.as_str();
        let held = event.is_held;
        if self.history.video_preview.has_focus(window, cx)
            && !(key == "f" && modifiers.control && !modifiers.alt && !modifiers.shift)
            && !(key == "escape" && plain)
        {
            return;
        }
        let handled = match key {
            "f" if modifiers.control && !modifiers.alt && !modifiers.shift => {
                window.focus(&self.history.search.read(cx).focus_handle);
                true
            }
            "f5" if plain => {
                if !held {
                    self.history_refresh(cx);
                }
                true
            }
            "escape" if plain && !searching && self.history_tools_escape(cx) => true,
            "enter" if plain && !searching && self.history.tools.pick.is_some() => {
                if !held {
                    self.history_pick_go(cx);
                }
                true
            }
            "a" if modifiers.control && !modifiers.alt && !modifiers.shift && !searching => {
                self.history_select_all(cx);
                true
            }
            "escape" if plain => {
                if !self.history.search.read(cx).text().is_empty() {
                    self.history
                        .search
                        .update(cx, |field, cx| field.set_text(String::new(), cx));
                } else if searching {
                    window.focus(&self.history.focus);
                } else {
                    window.focus(&self.history.focus);
                    self.history_set_single(None, cx);
                }
                cx.notify();
                true
            }
            "z" if modifiers.control && !modifiers.alt && !modifiers.shift && !searching => {
                if !self.history.pending.is_empty() {
                    self.history_undo(cx);
                }
                true
            }
            "c" if modifiers.control && !modifiers.alt && !modifiers.shift && !searching => {
                if !held {
                    self.history_copy(window, cx);
                }
                true
            }
            _ if searching || !plain => false,
            "space"
                if matches!(
                    self.history_selected().map(|item| &item.source),
                    Some(Source::Shot(_))
                ) && crate::image_quick_preview::decide(
                    key,
                    modifiers,
                    false,
                    crate::image_quick_preview::SpaceContext {
                        source: crate::image_quick_preview::HistorySource::Shot,
                        image_preview_enabled: true,
                        text_input_focused: searching,
                        composing: false,
                    },
                ) == Some(crate::image_quick_preview::SpaceAction::ViewImage) =>
            {
                if !held {
                    self.history_view_internal(cx);
                }
                true
            }
            "space"
                if matches!(
                    self.history_selected().map(|item| &item.source),
                    Some(Source::Recording(_))
                ) =>
            {
                if !held {
                    self.history_play_internal(cx);
                }
                true
            }
            "left" => {
                self.history_move(-1, cx);
                true
            }
            "right" => {
                self.history_move(1, cx);
                true
            }
            "up" => {
                self.history_move(-(self.history.row_hint.max(1) as isize), cx);
                true
            }
            "down" => {
                self.history_move(self.history.row_hint.max(1) as isize, cx);
                true
            }
            "home" => {
                self.history_move(isize::MIN, cx);
                true
            }
            "end" => {
                self.history_move(isize::MAX, cx);
                true
            }
            "enter" => {
                if !held {
                    self.history_primary(cx);
                }
                true
            }
            "delete" => {
                if !held {
                    self.history_delete(cx);
                }
                true
            }
            _ => false,
        };
        if handled {
            cx.stop_propagation();
        }
    }

    pub(super) fn history_body(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if self.history.recovery.visible {
            self.history_clear_video_preview(cx);
            return self.history_recovery_body(cx).into_any_element();
        }
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let visible = self.history_visible(cx);
        let viewport = f32::from(window.viewport_size().width);
        // 内容区左右各 32 内边距，另留滚动条宽度；网格不足 560 时收起详情栏。
        let full_width = (viewport - super::sidebar_width(window) - 64. - 12.).max(160.);
        let detail_fits = full_width - DETAIL_WIDTH >= 560.;
        let detail = self.history.detail && detail_fits;
        let native = crate::gpui_capture::hwnd(window);
        let preview_visible = unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible};
            IsWindowVisible(native).as_bool() && !IsIconic(native).as_bool()
        };
        let preview_selection = if detail
            && preview_visible
            && !self.media_popup.is_open()
            && !self.history_multi()
            && self.history.tools.pick.is_none()
        {
            visible.iter().find_map(|&index| {
                let item = &self.history.items[index];
                if self.history.selected.as_deref() != Some(item.key.as_str()) {
                    return None;
                }
                match &item.source {
                    Source::Recording(entry) => Some((item.key.clone(), entry.path.clone())),
                    _ => None,
                }
            })
        } else {
            None
        };
        self.history.video_preview.reconcile_with_focus_return(
            preview_selection
                .as_ref()
                .map(|(key, path)| (key.as_str(), path.as_path())),
            &self.history.focus,
            window,
            cx,
        );
        let grid_width = if detail {
            full_width - DETAIL_WIDTH
        } else {
            full_width
        };

        let mut segments = widgets::segmented(surface);
        for filter in Filter::ALL {
            let count = self
                .history
                .items
                .iter()
                .filter(|item| {
                    filter.accepts(item) && !self.history.pending.iter().any(|p| p.key == item.key)
                })
                .count();
            segments = segments.child(
                widgets::segment(
                    SharedString::from(format!("history-filter-{filter:?}")),
                    format!("{} {count}", filter.title()),
                    self.history.filter == filter,
                    palette,
                    surface,
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.history.filter = filter;
                    view.history_preview_tick(true, cx);
                    cx.notify();
                })),
            );
        }
        let toolbar = div()
            .flex_shrink_0()
            .px_8()
            .pb_3()
            .flex()
            .items_center()
            .gap_3()
            .child(segments)
            .child(div().flex_1())
            .when(self.history.filter == Filter::Recordings, |bar| {
                bar.child(
                    widgets::button(
                        "history-recording-folder",
                        crate::tr!("打开录屏目录", "Open recording folder"),
                        ButtonKind::Secondary,
                        palette,
                        surface,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        let folder = crate::recording::library_dir();
                        if let Err(error) = recordings::open(&folder, OpenAction::Folder) {
                            view.status = crate::trf!(
                                "无法打开录屏目录：{error}",
                                "Cannot open the folder: {error}"
                            );
                            cx.notify();
                        }
                    })),
                )
            })
            .child(
                div()
                    .w(px(220.))
                    .flex_shrink_0()
                    .child(self.history.search.clone()),
            )
            .child(
                widgets::icon_button(
                    "history-refresh",
                    Icon::Refresh,
                    !self.history.loading,
                    palette,
                )
                .on_click(cx.listener(|view, _, _, cx| view.history_refresh(cx))),
            )
            .child(self.history_tools_trigger(tools::MenuOrigin::Toolbar, cx))
            .when(detail_fits, |bar| {
                bar.child(
                    widgets::icon_button("history-detail-toggle", Icon::Sliders, true, palette)
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.history.detail = !view.history.detail;
                            view.history_preview_tick(true, cx);
                            cx.notify();
                        })),
                )
            });

        let mut notices: Vec<AnyElement> = Vec::new();
        notices.extend(self.history_pick_banner(cx));
        // 打开外部文件已并入“工具”菜单；这里只留实验期的未完成录制入口。
        if crate::file_launch_command::experimental_media_enabled() {
            notices.push(
                notice(
                    crate::tr!(
                        "未完成的录制可在这里检查与另存",
                        "Check and save unfinished recordings"
                    ),
                    palette,
                    surface,
                )
                .flex_wrap()
                .child(
                    widgets::button(
                        "history-unfinished-recordings",
                        crate::tr!("未完成录制…", "Unfinished recordings…"),
                        ButtonKind::Secondary,
                        palette,
                        surface,
                    )
                    .child(self.settings_probe("history-recovery-open"))
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.history_clear_video_preview(cx);
                        window.focus(&view.history.focus);
                        view.history_recovery_open(cx);
                    })),
                )
                .into_any_element(),
            );
        }
        if !self.preferences.history {
            notices.push(
                notice(
                    crate::tr!(
                        "截图历史未开启：新的截图不会记录，已有的截图、贴图和录屏照常显示。",
                        "Capture history is off: new captures aren't recorded; existing items still show."
                    ),
                    palette,
                    surface,
                )
                .child(
                    widgets::button(
                        "history-enable",
                        crate::tr!("开启", "Turn on"),
                        ButtonKind::Secondary,
                        palette,
                        surface,
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.history_enable_recording(cx))),
                )
                .into_any_element(),
            );
        }
        for warning in &self.history.warnings {
            notices.push(notice(warning.clone(), palette, surface).into_any_element());
        }

        let grid = self.history_grid(&visible, grid_width, cx);
        let mut left = div()
            .relative()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .children(notices.into_iter().map(|n| div().px_8().pb_3().child(n)))
            .child(grid);
        let action_bar = self.history_action_bar(cx);
        if !self.history.pending.is_empty() || action_bar.is_some() {
            let count = self.history.pending.len();
            left = left.child(
                div()
                    .absolute()
                    .left(px(0.))
                    .right(px(0.))
                    .bottom(px(16.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .when(count > 0, |bottom| {
                        bottom.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .pl_4()
                                .pr_2()
                                .py_2()
                                .rounded_lg()
                                .border_1()
                                .border_color(rgb(surface.line))
                                .bg(rgb(surface.raised))
                                .shadow_lg()
                                .text_size(px(13.))
                                .child(crate::trf!("已删除 {} 项", "Deleted {} items", count))
                                .child(
                                    div()
                                        .id("history-undo")
                                        .px_2()
                                        .py_1()
                                        .rounded_md()
                                        .cursor_pointer()
                                        .text_color(rgb(palette.accent))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .hover(move |style| style.bg(rgb(palette.hover)))
                                        .child(crate::tr!("撤销 Ctrl+Z", "Undo Ctrl+Z"))
                                        .on_click(
                                            cx.listener(|view, _, _, cx| view.history_undo(cx)),
                                        ),
                                ),
                        )
                    })
                    .children(action_bar),
            );
        }
        div()
            .id("history-page")
            .size_full()
            .flex()
            .overflow_hidden()
            .track_focus(&self.history.focus)
            .on_key_down(cx.listener(Self::history_key))
            .child(left)
            .when(detail, |root| {
                root.child(self.history_detail(preview_selection.is_some(), cx))
            })
            .into_any_element()
    }

    fn history_grid(
        &mut self,
        visible: &[usize],
        width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let mut grid = div()
            .id("history-grid")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.history.scroll)
            .px_8()
            .pb_8()
            .flex()
            .flex_col();
        self.history.child_index.clear();
        if visible.is_empty() {
            let text = if self.history.loading && self.history.items.is_empty() {
                crate::tr!("正在读取…", "Loading…")
            } else if !self.history.search.read(cx).text().trim().is_empty() {
                crate::tr!(
                    "没有匹配的条目。可以搜索标题、日期（2026-10-19、昨天）或尺寸（1920x1080）。",
                    "No matches. Try a title, a date (2026-10-19, Yesterday) or a size (1920x1080)."
                )
            } else {
                crate::tr!(
                    "还没有记录。截图、贴图和录屏完成后会出现在这里。",
                    "Nothing yet. Captures, pins and recordings appear here once finished."
                )
            };
            return grid
                .child(
                    div()
                        .pt_16()
                        .flex()
                        .justify_center()
                        .text_size(px(13.))
                        .text_color(rgb(palette.muted))
                        .child(text),
                )
                .into_any_element();
        }
        let today = library::local_time(SystemTime::now());
        let mut children = 0;
        let mut first_row = true;
        let mut start = 0;
        while start < visible.len() {
            let local = library::local_time(self.history.items[visible[start]].created);
            let days = local.map(|l| l.days);
            let mut end = start + 1;
            while end < visible.len()
                && library::local_time(self.history.items[visible[end]].created).map(|l| l.days)
                    == days
            {
                end += 1;
            }
            let label = match (local, today) {
                (Some(local), Some(today)) => library::day_label(&local, &today),
                _ => crate::tr!("日期未知", "Unknown date").to_owned(),
            };
            grid = grid.child(
                div()
                    .flex_shrink_0()
                    .pt_3()
                    .pb_2()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(palette.muted))
                            .child(crate::trf!("{} 项", "{} items", end - start)),
                    ),
            );
            children += 1;
            let group = &visible[start..end];
            let aspects: Vec<f32> = group
                .iter()
                .map(|&index| aspect(&self.history.items[index]))
                .collect();
            for row in library::justify(&aspects, width, ROW_HEIGHT, GAP) {
                if first_row {
                    self.history.row_hint = row.end - row.start;
                    first_row = false;
                }
                let mut line = div().flex_shrink_0().flex().gap(px(GAP)).mb(px(GAP));
                for (offset, &index) in group[row.start..row.end].iter().enumerate() {
                    let card_width =
                        (library::aspect_clamp(aspects[row.start + offset]) * row.height).floor();
                    self.history
                        .child_index
                        .insert(self.history.items[index].key.clone(), children);
                    line = line.child(self.history_card(index, card_width, row.height.floor(), cx));
                }
                grid = grid.child(line);
                children += 1;
            }
            start = end;
        }
        grid.into_any_element()
    }

    fn history_card(
        &self,
        index: usize,
        width: f32,
        height: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let item = &self.history.items[index];
        let order = self.history.chosen.iter().position(|key| *key == item.key);
        let selected =
            order.is_some() || self.history.selected.as_deref() == Some(item.key.as_str());
        let pick = self.history.tools.pick.as_ref().map(|pick| pick.tool);
        // 选择模式里不接受的条目淡出且不可勾选；多选与选择模式显示选择顺序。
        let refused =
            pick.is_some_and(|tool| !tool.takes(tools::is_image(item), tools::is_recording(item)));
        let mark = match order {
            Some(order) if pick.is_some() || self.history.chosen.len() > 1 => Some(
                div()
                    .absolute()
                    .top(px(6.))
                    .right(px(6.))
                    .min_w(px(22.))
                    .h(px(22.))
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(rgb(palette.accent))
                    .text_color(rgb(surface.on_accent))
                    .text_size(px(12.))
                    .font_weight(FontWeight::BOLD)
                    .child((order + 1).to_string()),
            ),
            None if pick.is_some() && !refused => Some(
                div()
                    .absolute()
                    .top(px(6.))
                    .right(px(6.))
                    .size(px(22.))
                    .rounded_full()
                    .border_2()
                    .border_color(rgb(0xe6edf3))
                    .bg(rgba(0x0c0f1360)),
            ),
            _ => None,
        };
        let key = item.key.clone();
        let mut badges = div().absolute().top(px(6.)).left(px(6.)).flex().gap_1();
        match &item.source {
            Source::Pin { state, text, .. } => {
                let (label, dot) = match state {
                    PinState::Live => (crate::tr!("屏幕上", "On screen"), 0x40ce9d),
                    PinState::Idle => (crate::tr!("未打开", "Not open"), 0xe6b450),
                    PinState::Closed => (crate::tr!("已关闭", "Closed"), 0x8997a6),
                };
                badges = badges.child(pill(label, Some(dot)));
                if *text {
                    badges = badges.child(pill(crate::tr!("文字", "Text"), None));
                }
            }
            Source::Recording(_) => {
                let text = item
                    .duration()
                    .map_or_else(|| "MP4".to_owned(), library::duration_text);
                badges = badges.child(pill(text, None));
            }
            Source::Shot(_) => {}
        }
        // 夹取范围内的比例填满卡片；过宽或过长的完整显示，免得只剩中间一段被放大。
        let real = aspect(item);
        let fit = if (library::aspect_clamp(real) - real).abs() > 0.01 {
            ObjectFit::Contain
        } else {
            ObjectFit::Cover
        };
        let body: AnyElement = match self.history.thumbs.get(&item.key) {
            Some(image) => img(image.clone())
                .size_full()
                .object_fit(fit)
                .into_any_element(),
            None if library::pin_preview_text(&item.source).is_some() => div()
                .size_full()
                .pt_8()
                .px_4()
                .pb_3()
                .overflow_hidden()
                .text_size(px(13.))
                .child(library::pin_preview_text(&item.source).unwrap_or_default())
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .bg(rgb(surface.track))
                .child(
                    crate::toolbar_icons::action(
                        match &item.source {
                            Source::Recording(_) => Icon::Video,
                            Source::Pin { text: true, .. } => Icon::Text,
                            Source::Pin { .. } => Icon::Pin,
                            Source::Shot(_) => Icon::Capture,
                        },
                        palette.muted,
                    )
                    .size(px(22.)),
                )
                .child(
                    div()
                        .max_w(px((width - 16.).max(20.)))
                        .px_2()
                        .text_size(px(12.))
                        .text_color(rgb(palette.muted))
                        .text_ellipsis()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(item.display_title()),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                        .child(if self.history.thumb_failed.contains(&item.key) {
                            crate::tr!("预览加载失败 · F5 重试", "Preview failed · F5 to retry")
                        } else if self.history.thumb_pending.contains_key(&item.key) {
                            crate::tr!("正在生成预览…", "Loading preview…")
                        } else {
                            crate::tr!("暂无可预览内容", "No preview available")
                        }),
                )
                .into_any_element(),
        };
        div()
            .id(SharedString::from(format!("history-card-{index}")))
            .relative()
            .w(px(width.max(24.)))
            .h(px(height.max(24.)))
            .flex_shrink_0()
            .rounded_md()
            .overflow_hidden()
            .cursor_pointer()
            .bg(rgb(surface.card))
            .border_2()
            .border_color(rgb(if selected {
                palette.accent
            } else {
                surface.line
            }))
            .when(!selected, |card| {
                card.hover(move |style| style.border_color(rgb(palette.muted)))
            })
            .when(refused, |card| card.opacity(0.3))
            .child(body)
            .child(badges)
            .children(mark)
            .on_click(cx.listener(move |view, event: &ClickEvent, window, cx| {
                view.history_card_click(key.clone(), event, window, cx);
            }))
            .into_any_element()
    }

    fn history_detail(&self, video_preview: bool, cx: &mut Context<Self>) -> AnyElement {
        let palette = crate::theme::palette(cx);
        let surface = widgets::surface(cx);
        let pane = div()
            .id("history-detail")
            .w(px(DETAIL_WIDTH))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(rgb(surface.line))
            .px_4()
            .pb_6()
            .flex()
            .flex_col()
            .gap_4();
        if self.history_multi() {
            return self.history_detail_multi(pane, cx);
        }
        let Some(item) = self.history_selected() else {
            return pane
                .pt_16()
                .items_center()
                .text_size(px(13.))
                .text_color(rgb(palette.muted))
                .child(crate::tr!(
                    "选中截图，工具就在手边",
                    "Select captures to use tools on them"
                ))
                .child(div().text_size(px(12.)).text_center().child(crate::tr!(
                    "Ctrl／Shift 多选后可对照、拼接、转 PDF；也可以先点“工具”再挑图。双击或 Enter 贴出／打开 · Delete 删除",
                    "Ctrl/Shift-click to compare, join or make a PDF, or pick a tool first. Double-click or Enter to pin/open · Delete"
                )))
                .into_any_element();
        };
        let preview = div()
            .h(px(if video_preview { 250. } else { 180. }))
            .flex_shrink_0()
            .rounded_lg()
            .overflow_hidden()
            .border_1()
            .border_color(rgb(surface.line))
            .bg(rgb(surface.track))
            .flex()
            .items_center()
            .justify_center()
            .map(|preview| {
                if video_preview {
                    return preview.child(self.history.video_preview.element(cx));
                }
                match self.history.thumbs.get(&item.key) {
                    Some(image) => preview.child(
                        img(image.clone())
                            .size_full()
                            .object_fit(ObjectFit::Contain),
                    ),
                    None if library::pin_preview_text(&item.source).is_some() => preview.child(
                        div()
                            .size_full()
                            .p_4()
                            .overflow_hidden()
                            .text_size(px(13.))
                            .child(library::pin_preview_text(&item.source).unwrap_or_default()),
                    ),
                    None => preview.child(
                        div()
                            .w_full()
                            .min_w(px(0.))
                            .p_4()
                            .text_size(px(12.))
                            .text_center()
                            .text_color(rgb(palette.muted))
                            .child(if self.history.thumb_failed.contains(&item.key) {
                                crate::tr!(
                                    "预览加载失败，按 F5 重试",
                                    "Preview failed. Press F5 to retry"
                                )
                            } else if self.history.thumb_pending.contains_key(&item.key) {
                                crate::tr!("正在生成预览…", "Loading preview…")
                            } else {
                                crate::tr!("暂无可预览内容", "No preview available")
                            }),
                    ),
                }
            });
        let mut facts: Vec<(&str, String)> =
            vec![(crate::tr!("类型", "Type"), item.kind_name().to_owned())];
        if let Some(local) = library::local_time(item.created) {
            facts.push((crate::tr!("时间", "Time"), library::date_text(&local)));
        }
        if let Some((w, h)) = item.size {
            let label = if matches!(item.source, Source::Pin { .. }) {
                crate::tr!("窗口", "Window")
            } else {
                crate::tr!("尺寸", "Size")
            };
            facts.push((label, format!("{w} × {h}")));
        }
        if let Some(duration) = item.duration() {
            facts.push((
                crate::tr!("时长", "Length"),
                library::duration_text(duration),
            ));
        }
        if let Some(bytes) = item.bytes {
            facts.push((crate::tr!("大小", "File size"), library::bytes_text(bytes)));
        }
        if let Source::Pin { state, group, .. } = &item.source {
            facts.push((
                crate::tr!("状态", "State"),
                match state {
                    PinState::Live => crate::tr!("在屏幕上", "On screen"),
                    PinState::Idle => crate::tr!("有记录、窗口未打开", "Recorded, not open"),
                    PinState::Closed => crate::tr!("已关闭，可恢复", "Closed, restorable"),
                }
                .to_owned(),
            ));
            if !group.is_empty() {
                facts.push((crate::tr!("分组", "Group"), group.clone()));
            }
        }
        if let Some(path) = item.path() {
            facts.push((
                crate::tr!("文件", "File"),
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        let mut table = div().flex().flex_col().gap_1().text_size(px(12.));
        for (label, value) in facts {
            table = table.child(
                div()
                    .flex()
                    .gap_3()
                    .child(
                        div()
                            .w(px(56.))
                            .flex_shrink_0()
                            .text_color(rgb(palette.muted))
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(value),
                    ),
            );
        }
        let button = |id: &'static str, label: &'static str, kind: ButtonKind| {
            widgets::button(id, label, kind, palette, surface).w_full()
        };
        let mut actions = div().flex().flex_col().gap_2();
        match &item.source {
            Source::Shot(_) => {
                actions = actions
                    .child(
                        button(
                            "history-pin",
                            crate::tr!("贴出", "Pin"),
                            ButtonKind::Primary,
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.history_primary(cx))),
                    )
                    .child(
                        button(
                            "history-copy",
                            crate::tr!("复制图片", "Copy image"),
                            ButtonKind::Secondary,
                        )
                        .on_click(cx.listener(|view, _, window, cx| view.history_copy(window, cx))),
                    )
                    .child(
                        button(
                            "history-edit",
                            crate::tr!("编辑", "Edit"),
                            ButtonKind::Secondary,
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.history_edit(cx))),
                    )
                    .child(
                        button(
                            "history-view-internal",
                            crate::tr!("查看图片", "View image"),
                            ButtonKind::Secondary,
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.history_view_internal(cx))),
                    )
                    .child(
                        button(
                            "history-reveal",
                            crate::tr!("在文件夹中显示", "Show in folder"),
                            ButtonKind::Secondary,
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.history_reveal(cx))),
                    );
            }
            Source::Pin {
                state: PinState::Live,
                ..
            } => {
                actions = actions.child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(palette.muted))
                        .child(crate::tr!(
                            "这张贴图正在屏幕上，可在“贴图”页管理。",
                            "This pin is on screen; manage it on the Pins page."
                        )),
                );
            }
            Source::Pin { .. } => {
                actions = actions.child(
                    button(
                        "history-restore",
                        crate::tr!("恢复贴图", "Restore pin"),
                        ButtonKind::Primary,
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.history_primary(cx))),
                );
            }
            Source::Recording(_) => {
                actions = actions
                    .child(
                        button(
                            "history-play",
                            crate::tr!("内置播放", "Play here"),
                            ButtonKind::Primary,
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.history_primary(cx))),
                    )
                    .child(
                        button(
                            "history-open-system",
                            crate::tr!("用系统应用打开", "Open in system app"),
                            ButtonKind::Secondary,
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.history_open_system(cx))),
                    )
                    .child(
                        button(
                            "history-copy-file",
                            crate::tr!("复制文件", "Copy file"),
                            ButtonKind::Secondary,
                        )
                        .on_click(cx.listener(|view, _, window, cx| view.history_copy(window, cx))),
                    )
                    .child(
                        button(
                            "history-reveal",
                            crate::tr!("在文件夹中显示", "Show in folder"),
                            ButtonKind::Secondary,
                        )
                        .on_click(cx.listener(|view, _, _, cx| view.history_reveal(cx))),
                    );
            }
        }
        if tools::is_image(item) || tools::is_recording(item) {
            actions = actions.child(self.history_tools_trigger(tools::MenuOrigin::Detail, cx));
        }
        if item.can_delete() {
            let label = if matches!(item.source, Source::Recording(_)) {
                crate::tr!("删除（移到回收站）", "Delete (to Recycle Bin)")
            } else {
                crate::tr!("删除", "Delete")
            };
            actions = actions.child(
                widgets::button(
                    "history-delete",
                    label,
                    ButtonKind::Secondary,
                    palette,
                    surface,
                )
                .w_full()
                .text_color(rgb(palette.danger))
                .on_click(cx.listener(|view, _, _, cx| view.history_delete(cx))),
            );
        }
        pane.pt_1()
            .child(preview)
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(item.display_title()),
            )
            .child(table)
            .child(actions)
            .into_any_element()
    }
}

fn pill(text: impl Into<SharedString>, dot: Option<u32>) -> gpui::Div {
    div()
        .h(px(18.))
        .px(px(6.))
        .flex()
        .items_center()
        .gap_1()
        .rounded_full()
        .bg(rgba(0x0c0f13c0))
        .text_color(rgb(0xe6edf3))
        .text_size(px(11.))
        .when_some(dot, |pill, color| {
            pill.child(div().size(px(6.)).rounded_full().bg(rgb(color)))
        })
        .child(text.into())
}

fn notice(
    text: impl Into<SharedString>,
    palette: crate::capture_toolbar::Palette,
    surface: widgets::Surface,
) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .py_2()
        .rounded_lg()
        .border_1()
        .border_color(rgb(surface.line))
        .bg(rgb(surface.card))
        .text_size(px(12.))
        .text_color(rgb(palette.muted))
        .child(div().flex_1().min_w(px(0.)).child(text.into()))
}

#[cfg(test)]
#[path = "main_history_video_smoke.rs"]
mod video_smoke;
