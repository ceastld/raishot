//! 原位敏感文字候选：仅显式打开时分类，Chrome 预览不进入图片／文档。
use super::*;
use crate::capture_ocr_overlay::{
    inline_click::ClickSlot, pointer_click_proof, pointer_down_proof,
};
use crate::{
    annotation::Tool,
    ocr_sensitive::{Preview, PreviewNavigation, Snapshot},
    ocr_sensitive_effect::SensitiveEffectEdit,
};

/// Arc<()> 必须按地址比较：相同正文的新安装也不能承继旧候选或按钮。
#[derive(Clone)]
struct SensitiveSource {
    target: OcrTarget,
    body: Arc<()>,
    frame: (u64, u64),
    installation: u64,
    overlay: Arc<()>,
}
impl PartialEq for SensitiveSource {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target
            && self.frame == other.frame
            && self.installation == other.installation
            && Arc::ptr_eq(&self.body, &other.body)
            && Arc::ptr_eq(&self.overlay, &other.overlay)
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum SensitiveIntent {
    Open,
    Toggle(usize),
    Apply(Tool),
    Close,
}

#[derive(Clone)]
struct SensitiveStamp {
    source: SensitiveSource,
    preview: Option<Arc<()>>,
    offset: gpui::Point<gpui::Pixels>,
    viewport: [f32; 2],
    scale: f32,
    projection: crate::ocr_projection::OcrProjection,
    projection_epoch: Arc<()>,
    theme: Arc<()>,
}
impl SensitiveStamp {
    fn same_layout_source(&self, current: &Self) -> bool {
        self.source == current.source
            && self.offset == current.offset
            && self.viewport == current.viewport
            && self.scale == current.scale
            && self.projection == current.projection
            && Arc::ptr_eq(&self.projection_epoch, &current.projection_epoch)
            && Arc::ptr_eq(&self.theme, &current.theme)
    }
    fn matches(&self, current: &Self) -> bool {
        self.same_layout_source(current)
            && match (&self.preview, &current.preview) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SensitiveKey {
    Navigate(PreviewNavigation),
    Toggle,
    SelectAll,
    Cancel,
}
fn sensitive_key(key: &str, m: gpui::Modifiers) -> Option<SensitiveKey> {
    if !m.modified() {
        Some(match key {
            "up" => SensitiveKey::Navigate(PreviewNavigation::Previous),
            "down" => SensitiveKey::Navigate(PreviewNavigation::Next),
            "home" => SensitiveKey::Navigate(PreviewNavigation::First),
            "end" => SensitiveKey::Navigate(PreviewNavigation::Last),
            "space" => SensitiveKey::Toggle,
            "escape" => SensitiveKey::Cancel,
            _ => return None,
        })
    } else if key == "a" && m.control && !m.shift && !m.alt && !m.platform && !m.function {
        Some(SensitiveKey::SelectAll)
    } else {
        None
    }
}
fn sensitive_key_repeat(key: SensitiveKey, held: bool) -> bool {
    !held || matches!(key, SensitiveKey::Navigate(_))
}
struct SensitiveKeyboard {
    stamp: SensitiveStamp,
    session: Arc<()>,
    key: SensitiveKey,
    focus: gpui::FocusHandle,
    _blur: gpui::Subscription,
    _activation: gpui::Subscription,
}

struct SensitiveClick {
    stamp: SensitiveStamp,
    intent: SensitiveIntent,
    focus: gpui::FocusHandle,
    _blur: gpui::Subscription,
    _activation: gpui::Subscription,
}

pub(super) struct SensitiveState {
    snapshot: Option<Snapshot>,
    installation: Option<SensitiveSource>,
    identity: Arc<()>,
    preview: Option<Preview<SensitiveSource>>,
    painted: Rc<RefCell<Option<SensitiveStamp>>>,
    click: Rc<RefCell<ClickSlot<SensitiveClick>>>,
    keyboard: Rc<RefCell<Option<SensitiveKeyboard>>>,
    session: Arc<()>,
    scroll: gpui::ScrollHandle,
}
impl SensitiveState {
    pub(super) fn new(recognized: &RecognizedText) -> Self {
        Self {
            // 只冻结完整 DTO／几何；此处不扫描敏感格式。
            snapshot: Snapshot::from_recognized(recognized),
            installation: None,
            identity: Arc::new(()),
            preview: None,
            painted: Default::default(),
            click: Default::default(),
            keyboard: Default::default(),
            session: Arc::new(()),
            scroll: gpui::ScrollHandle::new(),
        }
    }
    fn cancel(&mut self) {
        self.preview = None;
        self.painted.borrow_mut().take();
        self.click.borrow_mut().cancel();
        self.keyboard.borrow_mut().take();
        self.session = Arc::new(());
    }
}

/// 捕获的 canvas 必须仍属于当前 state 与完整帧，才可读写共享输入授权。
/// 旧回调的失败是中性的；只有当前帧发现输入不可用时才撤销本帧授权。
struct SensitivePaintReceipt {
    stamp: SensitiveStamp,
    painted: Rc<RefCell<Option<SensitiveStamp>>>,
    keyboard: Rc<RefCell<Option<SensitiveKeyboard>>>,
}
impl SensitivePaintReceipt {
    fn capture(state: &SensitiveState, stamp: SensitiveStamp) -> Self {
        Self {
            stamp,
            painted: Rc::clone(&state.painted),
            keyboard: Rc::clone(&state.keyboard),
        }
    }

    fn publish_current(
        &self,
        state: &SensitiveState,
        current: &SensitiveStamp,
        projection_epoch: &Arc<()>,
        accessible: bool,
    ) -> bool {
        if !Arc::ptr_eq(&self.stamp.projection_epoch, projection_epoch)
            || !self.stamp.matches(current)
            || !Rc::ptr_eq(&self.painted, &state.painted)
            || !Rc::ptr_eq(&self.keyboard, &state.keyboard)
        {
            return false;
        }
        *self.painted.borrow_mut() = accessible.then(|| self.stamp.clone());
        // 活动项的连续导航保留；有效主题、投影或来源变化不承继 held lease。
        if !accessible
            || self
                .keyboard
                .borrow()
                .as_ref()
                .is_some_and(|held| !held.stamp.same_layout_source(&self.stamp))
        {
            self.keyboard.borrow_mut().take();
        }
        accessible
    }
}

impl CaptureView {
    pub(super) fn bind_ocr_sensitive_installation(&mut self) {
        let Some(source) = self.ocr_sensitive_source() else {
            return;
        };
        if let Some(overlay) = &mut self.ocr_overlay {
            overlay.sensitive.installation = Some(source);
        }
    }

    fn ocr_sensitive_source(&self) -> Option<SensitiveSource> {
        let overlay = self.ocr_overlay.as_ref()?;
        Some(SensitiveSource {
            target: self.current_ocr_target()?,
            body: self.current_result_body_identity(),
            frame: self.ocr_find_result_identity(),
            installation: self.ocr_result_installation,
            overlay: Arc::clone(&overlay.sensitive.identity),
        })
    }

    pub(super) fn ocr_sensitive_available(&self) -> bool {
        let Some(source) = self.ocr_sensitive_source() else {
            return false;
        };
        self.result_editor.is_none()
            && self.ocr_overlay.as_ref().is_some_and(|overlay| {
                overlay.sensitive.installation.as_ref() == Some(&source)
                    && overlay.current(Some(&source.target), self.current_result_text())
                    && overlay
                        .sensitive
                        .snapshot
                        .as_ref()
                        .is_some_and(|snapshot| self.current_result_text() == Some(snapshot.text()))
            })
    }

    pub(super) fn cancel_ocr_sensitive_preview(&mut self) {
        if let Some(overlay) = &mut self.ocr_overlay {
            overlay.sensitive.cancel();
        }
    }

    pub(super) fn ocr_sensitive_preview_visible(&self) -> bool {
        self.ocr_overlay
            .as_ref()
            .is_some_and(|overlay| overlay.sensitive.preview.is_some())
    }

    pub(super) fn validate_ocr_sensitive_preview(&mut self) {
        if !self.ocr_sensitive_available() {
            self.cancel_ocr_sensitive_preview();
        }
    }

    fn ocr_sensitive_stamp(&self, window: &Window, cx: &gpui::App) -> Option<SensitiveStamp> {
        if !self.ocr_sensitive_available() {
            return None;
        }
        let overlay = self.ocr_overlay.as_ref()?;
        Some(SensitiveStamp {
            source: self.ocr_sensitive_source()?,
            preview: overlay
                .sensitive
                .preview
                .as_ref()
                .map(|preview| Arc::clone(preview.identity())),
            offset: self.canvas_offset(window),
            viewport: [
                f32::from(window.viewport_size().width),
                f32::from(window.viewport_size().height),
            ],
            scale: window.scale_factor(),
            projection: self.ocr_projection(window)?,
            projection_epoch: Arc::clone(&overlay.projection_epoch),
            theme: crate::theme::change_identity(cx)?,
        })
    }

    fn ocr_sensitive_access(&self, window: &Window, cx: &gpui::App, down: bool) -> bool {
        let capture = unsafe { GetCapture() };
        window.is_window_active()
            && crate::theme::change_identity(cx).is_some()
            && self.ocr_find_focus_owned(window, cx)
            && self.ocr_overlay_ready()
            && self.ocr_sensitive_available()
            && self.result_selection_pointer_ready()
            && self.text_copy_completion_is_idle()
            && !self.preview_text_replay
            && !self.color_picker_pending
            && self.ocr_overlay.as_ref().is_some_and(|overlay| {
                overlay.anchor.is_none()
                    && overlay.capture.is_none()
                    && overlay.keyboard_target_painted()
                    && self.ocr_projection(window).is_some_and(|projection| {
                        overlay.painted_projection.get() == Some(projection)
                    })
            })
            && (capture.0.is_null() || down && capture == hwnd(window))
    }

    fn ocr_sensitive_click_current(
        &self,
        stamp: &SensitiveStamp,
        window: &Window,
        cx: &gpui::App,
        down: bool,
    ) -> bool {
        self.ocr_sensitive_access(window, cx, down)
            && self
                .ocr_sensitive_stamp(window, cx)
                .is_some_and(|current| stamp.matches(&current))
            && self.ocr_overlay.as_ref().is_some_and(|overlay| {
                overlay
                    .sensitive
                    .painted
                    .borrow()
                    .as_ref()
                    .is_some_and(|painted| stamp.matches(painted))
            })
    }

    /// 冻结键所有权后再验证来源：旧帧/失焦只能拒绝动作，不能移选区或开工具栏。
    pub(super) fn ocr_sensitive_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(key) = sensitive_key(event.keystroke.key.as_str(), event.keystroke.modifiers)
            .filter(|_| self.ocr_sensitive_preview_visible())
        else {
            return false;
        };
        cx.stop_propagation();
        self.validate_ocr_overlay();
        self.validate_ocr_sensitive_preview();
        let Some(stamp) = self.ocr_sensitive_stamp(window, cx) else {
            self.clear_ocr_sensitive_input_receipts();
            return true;
        };
        if !sensitive_key_repeat(key, event.is_held) {
            return true;
        }
        let ready = self.ocr_sensitive_access(window, cx, false)
            && self.focus.is_focused(window)
            && !crate::pin_view::find_ui::primary_button_down()
            && !self.result_selection_dragging(self.result_selection_generation())
            && self.ocr_overlay.as_ref().is_some_and(|overlay| {
                overlay
                    .sensitive
                    .painted
                    .borrow()
                    .as_ref()
                    .is_some_and(|painted| {
                        painted.preview.is_some()
                            && if matches!(key, SensitiveKey::Navigate(_)) {
                                // 活动项连按只要求同一次预览已真实显示，不等待每次选择新paint。
                                stamp.same_layout_source(painted)
                            } else {
                                stamp.matches(painted)
                            }
                    })
            });
        let Some(overlay) = &mut self.ocr_overlay else {
            return true;
        };
        let state = &mut overlay.sensitive;
        if !ready || state.preview.is_none() {
            state.keyboard.borrow_mut().take();
            state.click.borrow_mut().cancel();
            return true;
        }
        if event.is_held {
            let current = state.keyboard.borrow().as_ref().is_some_and(|held| {
                held.key == key
                    && held.focus.is_focused(window)
                    && Arc::ptr_eq(&held.session, &state.session)
                    && held.stamp.same_layout_source(&stamp)
            });
            if !current {
                state.keyboard.borrow_mut().take();
                return true;
            }
        } else {
            let Some(focus) = window.focused(cx) else {
                return true;
            };
            let keyboard = Rc::clone(&state.keyboard);
            let blur_keyboard = Rc::clone(&keyboard);
            let blur = cx.on_blur(&focus, window, move |_, _, cx| {
                blur_keyboard.borrow_mut().take();
                cx.notify();
            });
            let activation_keyboard = Rc::clone(&keyboard);
            let activation = cx.observe_window_activation(window, move |_, window, cx| {
                if !window.is_window_active() {
                    activation_keyboard.borrow_mut().take();
                    cx.notify();
                }
            });
            *keyboard.borrow_mut() = Some(SensitiveKeyboard {
                stamp,
                session: Arc::clone(&state.session),
                key,
                focus,
                _blur: blur,
                _activation: activation,
            });
        }
        state.click.borrow_mut().cancel();
        if key == SensitiveKey::Cancel {
            state.cancel();
        } else {
            let preview = state.preview.as_mut().expect("敏感预览已核验");
            match key {
                SensitiveKey::Navigate(navigation) => {
                    preview.navigate(navigation);
                }
                SensitiveKey::Toggle => {
                    preview.toggle_active();
                }
                SensitiveKey::SelectAll => {
                    preview.select_all();
                }
                SensitiveKey::Cancel => unreachable!(),
            }
            if let Some(active) = preview.active() {
                state.scroll.scroll_to_item(active);
            }
        }
        cx.notify();
        true
    }

    pub(super) fn clear_ocr_sensitive_input_receipts(&self) {
        if let Some(overlay) = &self.ocr_overlay {
            overlay.sensitive.click.borrow_mut().cancel();
            overlay.sensitive.keyboard.borrow_mut().take();
            overlay.sensitive.painted.borrow_mut().take();
        }
    }

    /// 通用捕获阶段只撤销旧鼠标意图，保留连续方向键与已显示的预览。
    pub(crate) fn cancel_ocr_sensitive_pointer_click(&self) {
        if let Some(overlay) = &self.ocr_overlay {
            overlay.sensitive.click.borrow_mut().cancel();
        }
    }

    fn arm_ocr_sensitive_click(
        &mut self,
        stamp: &SensitiveStamp,
        intent: SensitiveIntent,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(overlay) = &self.ocr_overlay else {
            return;
        };
        let click = Rc::clone(&overlay.sensitive.click);
        click.borrow_mut().cancel();
        if event.button != MouseButton::Left
            || event.first_mouse
            || !self.ocr_sensitive_click_current(stamp, window, cx, true)
        {
            return;
        }
        let Some(focus) = window.focused(cx) else {
            return;
        };
        let token = Arc::new(());
        let blur_click = Rc::downgrade(&click);
        let blur_token = Arc::clone(&token);
        let blur = cx.on_blur(&focus, window, move |_, _, _| {
            if let Some(click) = blur_click.upgrade() {
                click.borrow_mut().cancel_token(&blur_token);
            }
        });
        let activation_click = Rc::downgrade(&click);
        let activation_token = Arc::clone(&token);
        let activation = cx.observe_window_activation(window, move |_, window, _| {
            if !window.is_window_active() {
                if let Some(click) = activation_click.upgrade() {
                    click.borrow_mut().cancel_token(&activation_token);
                }
            }
        });
        click.borrow_mut().arm(
            token,
            "ocr-sensitive",
            pointer_down_proof(event),
            SensitiveClick {
                stamp: stamp.clone(),
                intent,
                focus,
                _blur: blur,
                _activation: activation,
            },
        );
    }

    fn finish_ocr_sensitive_click(
        &mut self,
        stamp: &SensitiveStamp,
        intent: SensitiveIntent,
        event: &gpui::ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (down, left_pair) = pointer_click_proof(event);
        // claim 首先消费槽；错误按钮配对或键盘 Click 不能留待未来重试。
        let click = self.ocr_overlay.as_ref().and_then(|overlay| {
            overlay
                .sensitive
                .click
                .borrow_mut()
                .claim("ocr-sensitive", down, left_pair, |saved| {
                    saved.intent == intent && saved.stamp.matches(stamp)
                })
        });
        let Some(click) = click else { return };
        if !click.focus.is_focused(window)
            || crate::pin_view::find_ui::primary_button_down()
            || !self.ocr_sensitive_click_current(&click.stamp, window, cx, false)
        {
            return;
        }
        match click.intent {
            SensitiveIntent::Open => self.open_ocr_sensitive_preview(window, cx),
            SensitiveIntent::Close => self.cancel_ocr_sensitive_preview(),
            SensitiveIntent::Toggle(id) => {
                if let Some(preview) = self
                    .ocr_overlay
                    .as_mut()
                    .and_then(|overlay| overlay.sensitive.preview.as_mut())
                {
                    preview.set_active(id);
                    preview.set_selected(id, !preview.selected(id));
                }
            }
            SensitiveIntent::Apply(effect) => {
                if let Err(error) =
                    self.apply_ocr_sensitive_preview(&click.stamp, effect, window, cx)
                {
                    self.status = trf!(
                        "敏感文字处理失败：{error}",
                        "Sensitive text failed: {error}"
                    );
                }
            }
        }
        cx.notify();
    }

    pub(super) fn bind_ocr_sensitive_button(
        &self,
        button: gpui::Stateful<gpui::Div>,
        intent: SensitiveIntent,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let Some(stamp) = self.ocr_sensitive_stamp(window, cx) else {
            return button;
        };
        let down = stamp.clone();
        button
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                    view.arm_ocr_sensitive_click(&down, intent, event, window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_click(
                cx.listener(move |view, event: &gpui::ClickEvent, window, cx| {
                    view.finish_ocr_sensitive_click(&stamp, intent, event, window, cx);
                    cx.stop_propagation();
                }),
            )
    }

    pub(super) fn open_ocr_sensitive_preview(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ocr_sensitive_access(window, cx, false) {
            return;
        }
        let Some(source) = self.ocr_sensitive_source() else {
            return;
        };
        self.clear_find_preview();
        self.cancel_pending_local_retry_click();
        self.ocr_find = None;
        if let Some(overlay) = &mut self.ocr_overlay {
            let Some(snapshot) = &overlay.sensitive.snapshot else {
                return;
            };
            // 唯一分类入口：不调用识别器，也不在 render / paint 中扫描正文。
            overlay.panel.close_menu();
            overlay.sensitive.preview = Some(Preview::new(source, snapshot));
            overlay.sensitive.painted.borrow_mut().take();
            overlay.sensitive.click.borrow_mut().cancel();
            overlay.sensitive.keyboard.borrow_mut().take();
            overlay.sensitive.session = Arc::new(());
            overlay.sensitive.scroll = gpui::ScrollHandle::new();
        }
        window.focus(&self.focus);
        self.status = tr!(
            "请检查敏感候选并逐项勾选，再确认马赛克或模糊；未勾选不处理",
            "Review and select candidates, then apply mosaic or blur; unselected items stay"
        )
        .into();
        cx.notify();
    }

    fn apply_ocr_sensitive_preview(
        &mut self,
        stamp: &SensitiveStamp,
        effect: Tool,
        window: &Window,
        cx: &gpui::App,
    ) -> crate::Result<()> {
        if !self.ocr_sensitive_click_current(stamp, window, cx, false) {
            self.cancel_ocr_sensitive_preview();
            return Ok(());
        }
        let Some(identity) = &stamp.preview else {
            return Ok(());
        };
        let Some(preview) = self
            .ocr_overlay
            .as_ref()
            .and_then(|overlay| overlay.sensitive.preview.as_ref())
        else {
            return Ok(());
        };
        let text = self.current_result_text().ok_or("敏感候选正文失效")?;
        let Some(regions) = preview.selected_regions(&stamp.source, text, identity) else {
            self.cancel_ocr_sensitive_preview();
            return Ok(());
        };
        // 可信局部区域必须完整位于本次 ROI；不裁掉溢出几何再冒充完整定位。
        let regions = world_regions(regions, stamp.source.target.selected)?;
        let style = crate::tool_presets::default_style(effect);
        let Some(edit) = SensitiveEffectEdit::prepare(
            &self.editor.document,
            stamp.source.clone(),
            Arc::clone(identity),
            regions,
            effect,
            style,
        )?
        else {
            return Ok(());
        };
        let Some(annotations) = edit.annotations(&self.editor.document) else {
            self.cancel_ocr_sensitive_preview();
            return Ok(());
        };
        let scene = self
            .prepared_scene()
            .updated(&annotations, &self.source_pixels());
        if !scene.errors.is_empty() {
            return Err(scene.errors.join(";").into());
        }
        let authorized = self.ocr_sensitive_click_current(stamp, window, cx, false);
        let Some(source) = self.ocr_sensitive_source() else {
            self.cancel_ocr_sensitive_preview();
            return Ok(());
        };
        let preview_identity = self
            .ocr_overlay
            .as_ref()
            .and_then(|overlay| overlay.sensitive.preview.as_ref())
            .map(|preview| Arc::clone(preview.identity()));
        let Some(preview_identity) = preview_identity else {
            return Ok(());
        };
        if edit.commit(
            &mut self.editor.document,
            &source,
            &preview_identity,
            authorized,
        ) {
            self.scene = Arc::new(scene);
            self.scene_stamp = None;
            // 效果改变下层，所有本次文字定位都重新识别；预览本身从未进入 Scene。
            self.clear_ocr_overlay();
            self.dismiss_recognition_result();
            self.status = tr!(
                "敏感文字区域已处理，可一次撤销",
                "Selected regions applied; one undo restores them"
            )
            .into();
        } else {
            self.cancel_ocr_sensitive_preview();
        }
        Ok(())
    }

    pub(super) fn render_ocr_sensitive(
        &self,
        scale: f32,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !scale.is_finite() || scale <= 0. || scale != window.scale_factor() {
            self.clear_ocr_sensitive_input_receipts();
            return None;
        }
        let overlay = self.ocr_overlay.as_ref()?;
        let Some(stamp) = self.ocr_sensitive_stamp(window, cx) else {
            self.clear_ocr_sensitive_input_receipts();
            return None;
        };
        let palette = crate::theme::palette(cx);
        let owner = cx.entity().downgrade();
        let paint_receipt = SensitivePaintReceipt::capture(&overlay.sensitive, stamp.clone());
        let preview = overlay.sensitive.preview.as_ref();
        let quads: Vec<_> = preview.map_or_else(Vec::new, |preview| {
            preview
                .candidates()
                .iter()
                .flat_map(|candidate| {
                    candidate.quads.iter().map(|quad| {
                        (
                            quad.map(|p| Point {
                                x: p.x + stamp.source.target.selected.x as f32,
                                y: p.y + stamp.source.target.selected.y as f32,
                            }),
                            preview.selected(candidate.id),
                        )
                    })
                })
                .collect()
        });
        let projection = stamp.projection;
        let roi = projection.rect(stamp.source.target.selected);
        let paint = canvas(
            |_, _, _| (),
            move |_, _, window, cx| {
                let current = owner.upgrade().is_some_and(|owner| {
                    let view = owner.read(cx);
                    let Some(overlay) = &view.ocr_overlay else {
                        return false;
                    };
                    if !overlay.accepts_projection_epoch(&paint_receipt.stamp.projection_epoch) {
                        return false;
                    }
                    let Some(current) = view.ocr_sensitive_stamp(window, cx) else {
                        return false;
                    };
                    paint_receipt.publish_current(
                        &overlay.sensitive,
                        &current,
                        &overlay.projection_epoch,
                        view.ocr_sensitive_access(window, cx, true),
                    )
                });
                if !current {
                    return;
                };
                window.with_content_mask(
                    Some(gpui::ContentMask {
                        bounds: Bounds::new(
                            point(px(roi.x), px(roi.y)),
                            size(px(roi.width), px(roi.height)),
                        ),
                    }),
                    |window| {
                        for (quad, selected) in &quads {
                            let vertices = quad.map(|p| {
                                let p = projection.point(p);
                                point(px(p.x), px(p.y))
                            });
                            let mut path = gpui::Path::new(vertices[0]);
                            let uv = point(0., 1.);
                            path.push_triangle(
                                (vertices[0], vertices[1], vertices[2]),
                                (uv, uv, uv),
                            );
                            path.push_triangle(
                                (vertices[0], vertices[2], vertices[3]),
                                (uv, uv, uv),
                            );
                            window.paint_path(
                                path,
                                rgba(if *selected { 0x378de55a } else { 0xf2aa2630 }),
                            );
                            let mut border = gpui::PathBuilder::stroke(px(1.5));
                            border.move_to(vertices[0]);
                            for vertex in vertices.iter().skip(1).chain(vertices.first()) {
                                border.line_to(*vertex);
                            }
                            if let Ok(border) = border.build() {
                                window.paint_path(
                                    border,
                                    rgb(if *selected {
                                        palette.accent
                                    } else {
                                        palette.foreground
                                    }),
                                );
                            }
                        }
                    },
                );
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        let mut root = div().absolute().top_0().left_0().size_full().child(paint);
        let work = self.ocr_ui_work(window)?;
        // 结果面板存在时候选清单盖在面板标题以下，入口改由面板底栏提供。
        let dock = self.ocr_panel_frame(window).map(|layout| layout.sheet());
        if let Some(preview) = preview {
            let (left, top, width, height) = dock.map_or_else(
                || {
                    let width = (work.width - 16.).clamp(1., 360.);
                    (
                        work.right() - width - 8.,
                        work.y + 8.,
                        width,
                        (work.height - 16.).clamp(1., 280.),
                    )
                },
                |sheet| (sheet.x, sheet.y, sheet.width, sheet.height),
            );
            let mut panel = div()
                .id("ocr-sensitive-preview")
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(width))
                .max_h(px(height))
                .p_2()
                .flex()
                .flex_col()
                .gap(px(6.))
                .bg(rgb(palette.background))
                .border_1()
                .border_color(rgb(palette.border))
                .rounded_md()
                .shadow_md()
                .text_size(px(12.))
                .text_color(rgb(palette.foreground))
                .overflow_hidden()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(tr!(
                    "敏感文字候选 · 逐项勾选",
                    "Sensitive candidates · select items"
                ))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(rgb(palette.muted))
                        .child(tr!(
                            "↑↓ / Home End 移动 · 空格勾选 · Ctrl+A全选 · Esc取消",
                            "↑↓ / Home End move · Space select · Ctrl+A all · Esc cancel"
                        )),
                );
            let mut list = div()
                .id("ocr-sensitive-items")
                .max_h(px((height - 112.).max(1.)))
                .overflow_y_scroll()
                .track_scroll(&overlay.sensitive.scroll)
                .flex()
                .flex_col()
                .gap(px(4.));
            if preview.candidates().is_empty() {
                list = list.child(tr!(
                    "没有能在图上定位的敏感文字",
                    "No sensitive text could be located in the image"
                ));
            }
            for candidate in preview.candidates() {
                let active = preview.active() == Some(candidate.id);
                let button = div()
                    .id(("ocr-sensitive-item", candidate.id))
                    .w_full()
                    .p_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(if active {
                        palette.accent
                    } else {
                        palette.border
                    }))
                    .overflow_hidden()
                    .text_size(px(12.))
                    .hover(move |s| s.bg(rgb(palette.hover)))
                    .child(preview.item_label(candidate));
                list = list.child(self.bind_ocr_sensitive_button(
                    button,
                    SensitiveIntent::Toggle(candidate.id),
                    window,
                    cx,
                ));
            }
            panel = panel.child(list);
            let selected = preview.selected_count();
            panel = panel.child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                    .child(trf!(
                        "已选 {selected} 项；取消不会改图",
                        "{selected} selected; cancel keeps image"
                    )),
            );
            let mut controls = div().flex().flex_wrap().gap(px(4.));
            for (id, label, intent, enabled) in [
                (
                    "ocr-sensitive-mosaic",
                    tr!("确认马赛克", "Apply mosaic"),
                    SensitiveIntent::Apply(Tool::Mosaic),
                    selected > 0,
                ),
                (
                    "ocr-sensitive-blur",
                    tr!("确认模糊", "Apply blur"),
                    SensitiveIntent::Apply(Tool::Blur),
                    selected > 0,
                ),
                (
                    "ocr-sensitive-close",
                    tr!("取消", "Cancel"),
                    SensitiveIntent::Close,
                    true,
                ),
            ] {
                let mut button = div()
                    .id(id)
                    .w(px(98_f32.min((width - 16.).max(1.))))
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_size(px(12.))
                    .text_color(rgb(if enabled {
                        palette.foreground
                    } else {
                        palette.muted
                    }))
                    .child(label);
                if enabled {
                    button = self.bind_ocr_sensitive_button(
                        button.hover(move |s| s.bg(rgb(palette.hover))),
                        intent,
                        window,
                        cx,
                    );
                }
                controls = controls.child(button);
            }
            root = root.child(panel.child(controls));
        } else if dock.is_none() && !overlay.nonempty_selection() && overlay.anchor.is_none() {
            // OCR 刚完成且尚未选字时也有真实可见入口；有选字时使用第十三项操作。
            let width = 96_f32.min((work.width - 12.).max(1.));
            let x = (roi.right() - width)
                .clamp(work.x + 6., (work.right() - width - 6.).max(work.x + 6.));
            let y = (roi.bottom() + 6.).clamp(work.y + 6., (work.bottom() - 34.).max(work.y + 6.));
            let button = div()
                .id("ocr-sensitive-open")
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(width))
                .h(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .bg(rgb(palette.background))
                .border_1()
                .border_color(rgb(palette.border))
                .text_size(px(12.))
                .text_color(rgb(palette.foreground))
                .whitespace_nowrap()
                .overflow_hidden()
                .hover(move |s| s.bg(rgb(palette.hover)))
                .child(tr!("敏感文字", "Sensitive"));
            root = root.child(self.bind_ocr_sensitive_button(
                button,
                SensitiveIntent::Open,
                window,
                cx,
            ));
        }
        Some(root.into_any_element())
    }
}

/// ROI 局部→全画布只在这里平移；世界负原点属于 source.canvas，不再叠加。
fn world_regions(
    mut regions: Vec<ShapeBounds>,
    roi: crate::geometry::Rect,
) -> crate::Result<Vec<ShapeBounds>> {
    for region in &mut regions {
        if !region.x.is_finite()
            || !region.y.is_finite()
            || !region.width.is_finite()
            || !region.height.is_finite()
            || region.x < 0.
            || region.y < 0.
            || region.width <= 0.
            || region.height <= 0.
            || region.x + region.width > roi.width as f32
            || region.y + region.height > roi.height as f32
        {
            return Err("敏感候选定位超出原识别区域".into());
        }
        region.x += roi.x as f32;
        region.y += roi.y as f32;
    }
    Ok(regions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Rect, ScreenRect};

    fn source() -> SensitiveSource {
        SensitiveSource {
            target: OcrTarget {
                canvas: ScreenRect {
                    left: -320,
                    top: -40,
                    width: 320,
                    height: 200,
                },
                selected: Rect {
                    x: 40,
                    y: 60,
                    width: 180,
                    height: 40,
                },
                revisions: (4, 7),
                stamp: (11, 13),
                identity: crate::scene::SourceIdentity::of(&crate::scene::SourcePixels {
                    fill: None,
                    canvas: Rect::full(320, 200),
                    planes: Vec::new(),
                    overlay: None,
                }),
            },
            body: Arc::new(()),
            frame: (2, 0),
            installation: 9,
            overlay: Arc::new(()),
        }
    }

    #[test]
    fn sensitive_capture_binding_rejects_same_bytes_new_installation_and_edited_back() {
        let original = source();
        assert!(original == original.clone());
        for field in 0..6 {
            let mut changed = original.clone();
            match field {
                0 => changed.body = Arc::new(()),
                1 => changed.overlay = Arc::new(()),
                2 => changed.frame.1 += 2, // 正文改动再改回也保留修订
                3 => changed.installation += 1,
                4 => changed.target.selected.x += 1,
                _ => changed.target.revisions.0 += 2, // Undo/Redo 回到相似外观
            }
            assert!(original != changed, "必须拒绝变更身份字段 {field}");
        }
    }

    #[test]
    fn sensitive_capture_selection_revision_invalidates_pressed_confirmation() {
        let rect = Rect {
            x: 4,
            y: 5,
            width: 160,
            height: 20,
        };
        let geometry = crate::recognition::TextGeometry {
            quad: [
                Point { x: 4., y: 5. },
                Point { x: 164., y: 5. },
                Point { x: 164., y: 25. },
                Point { x: 4., y: 25. },
            ],
        };
        let direction = crate::recognition::HorizontalDirection::LeftToRight;
        let text = "user@example.com";
        let recognized = RecognizedText {
            text: text.into(),
            regions: vec![rect],
            words: vec![crate::recognition::RecognizedWord {
                text: text.into(),
                rect,
                line: 0,
                horizontal_direction: direction,
                geometry: Some(geometry),
            }],
            lines: vec![crate::recognition::RecognizedLine {
                text: text.into(),
                rect: Some(rect),
                horizontal_direction: direction,
                geometry: Some(geometry),
                graphemes: None,
            }],
        };
        let source = source();
        let snapshot = Snapshot::from_recognized(&recognized).expect("完整真实 DTO");
        let mut preview = Preview::new(source.clone(), &snapshot);
        assert_eq!(preview.candidates().len(), 1);
        let pressed = SensitiveStamp {
            source,
            preview: Some(Arc::clone(preview.identity())),
            offset: point(px(0.), px(0.)),
            viewport: [320., 200.],
            scale: 1.,
            projection: crate::ocr_projection::OcrProjection::new(
                crate::viewport::Viewport::default(),
                1.,
                [320., 200.],
            )
            .unwrap(),
            projection_epoch: Arc::new(()),
            theme: Arc::new(()),
        };
        assert!(!preview.selected(0));
        assert!(preview.set_selected(0, true));
        let mut current = pressed.clone();
        current.preview = Some(Arc::clone(preview.identity()));
        assert!(!pressed.matches(&current));
        assert!(
            preview
                .selected_regions(&current.source, text, current.preview.as_ref().unwrap())
                .unwrap()
                .len()
                == 1
        );
        assert!(preview.set_selected(0, false));
        assert!(preview
            .selected_regions(&current.source, text, preview.identity())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn sensitive_capture_regions_translate_roi_once_and_reject_partial_outside_geometry() {
        let roi = source().target.selected;
        let local = ShapeBounds {
            x: 4.,
            y: 5.,
            width: 20.,
            height: 10.,
        };
        let world = world_regions(vec![local], roi).unwrap();
        assert_eq!(
            world,
            vec![ShapeBounds {
                x: 44.,
                y: 65.,
                ..local
            }]
        );
        assert!(world_regions(vec![ShapeBounds { x: -1., ..local }], roi).is_err());
        assert!(world_regions(vec![ShapeBounds { x: 179., ..local }], roi).is_err());
        assert!(world_regions(
            vec![ShapeBounds {
                height: f32::NAN,
                ..local
            }],
            roi
        )
        .is_err());
        assert!(world_regions(Vec::new(), roi).unwrap().is_empty());
    }

    #[test]
    fn sensitive_keyboard_exact_keys_do_not_claim_enter_or_modified_navigation() {
        let plain = gpui::Modifiers::default();
        for (key, expected) in [
            ("up", SensitiveKey::Navigate(PreviewNavigation::Previous)),
            ("down", SensitiveKey::Navigate(PreviewNavigation::Next)),
            ("home", SensitiveKey::Navigate(PreviewNavigation::First)),
            ("end", SensitiveKey::Navigate(PreviewNavigation::Last)),
            ("space", SensitiveKey::Toggle),
            ("escape", SensitiveKey::Cancel),
        ] {
            assert_eq!(sensitive_key(key, plain), Some(expected));
            assert_eq!(
                sensitive_key(
                    key,
                    gpui::Modifiers {
                        shift: true,
                        ..plain
                    }
                ),
                None
            );
        }
        let control = gpui::Modifiers {
            control: true,
            ..plain
        };
        assert_eq!(sensitive_key("a", control), Some(SensitiveKey::SelectAll));
        for modifiers in [
            plain,
            gpui::Modifiers {
                alt: true,
                ..control
            },
            gpui::Modifiers {
                shift: true,
                ..control
            },
            gpui::Modifiers {
                platform: true,
                ..control
            },
            gpui::Modifiers {
                function: true,
                ..control
            },
        ] {
            assert_eq!(sensitive_key("a", modifiers), None);
        }
        for key in ["enter", "left", "right", "h", "c"] {
            assert_eq!(sensitive_key(key, plain), None);
        }
    }

    #[test]
    fn sensitive_keyboard_only_navigation_accepts_held_events() {
        for key in [
            SensitiveKey::Navigate(PreviewNavigation::Previous),
            SensitiveKey::Navigate(PreviewNavigation::Next),
            SensitiveKey::Navigate(PreviewNavigation::First),
            SensitiveKey::Navigate(PreviewNavigation::Last),
            SensitiveKey::Toggle,
            SensitiveKey::SelectAll,
            SensitiveKey::Cancel,
        ] {
            assert!(sensitive_key_repeat(key, false));
            assert_eq!(
                sensitive_key_repeat(key, true),
                matches!(key, SensitiveKey::Navigate(_))
            );
        }
    }

    #[test]
    fn sensitive_keyboard_receipt_allows_activity_repeat_but_rejects_theme_and_source_changes() {
        let painted = SensitiveStamp {
            source: source(),
            preview: Some(Arc::new(())),
            offset: point(px(0.), px(0.)),
            viewport: [320., 200.],
            scale: 1.,
            projection: crate::ocr_projection::OcrProjection::new(
                crate::viewport::Viewport::default(),
                1.,
                [320., 200.],
            )
            .unwrap(),
            projection_epoch: Arc::new(()),
            theme: Arc::new(()),
        };
        let mut changed = painted.clone();
        changed.preview = Some(Arc::new(()));
        assert!(painted.same_layout_source(&changed));
        assert!(!painted.matches(&changed)); // 活动项新身份不能完成旧鼠标确认。
        for field in 0..7 {
            let mut changed = painted.clone();
            match field {
                0 => changed.source.body = Arc::new(()),
                1 => changed.source.installation += 1,
                2 => changed.source.overlay = Arc::new(()),
                3 => changed.offset.x += px(1.),
                4 => changed.viewport[1] += 1.,
                5 => changed.scale = 2.,
                _ => changed.theme = Arc::new(()),
            }
            assert!(!painted.same_layout_source(&changed));
        }
    }

    #[test]
    fn sensitive_theme_identity_aba_rejects_old_confirmation_and_held_layout() {
        let initial = SensitiveStamp {
            source: source(),
            preview: Some(Arc::new(())),
            offset: point(px(0.), px(0.)),
            viewport: [320., 200.],
            scale: 1.,
            projection: crate::ocr_projection::OcrProjection::new(
                crate::viewport::Viewport::default(),
                1.,
                [320., 200.],
            )
            .unwrap(),
            projection_epoch: Arc::new(()),
            theme: Arc::new(()),
        };
        // change_identity 的有效变化合同由theme模块实际update_values模型覆盖。
        // 这里验证真实Capture消费者，配色切回时来源/选区/预览全部原样。
        let mut current = initial.clone();
        current.theme = Arc::new(()); // Light→Dark / System有效变化。
        assert!(!initial.matches(&current));
        assert!(!initial.same_layout_source(&current));
        let intermediate = current.clone();
        current.theme = Arc::new(()); // Dark→Light / System light ABA。
        assert!(!initial.matches(&current));
        assert!(!initial.same_layout_source(&current));
        assert!(!intermediate.matches(&current));
        assert!(current.source.target == initial.source.target);
        assert!(Arc::ptr_eq(
            current.preview.as_ref().unwrap(),
            initial.preview.as_ref().unwrap()
        ));
    }

    #[test]
    fn sensitive_theme_identity_noop_preserves_new_current_paint_and_navigation() {
        let current = SensitiveStamp {
            source: source(),
            preview: Some(Arc::new(())),
            offset: point(px(0.), px(0.)),
            viewport: [320., 200.],
            scale: 1.,
            projection: crate::ocr_projection::OcrProjection::new(
                crate::viewport::Viewport::default(),
                1.,
                [320., 200.],
            )
            .unwrap(),
            projection_epoch: Arc::new(()),
            theme: Arc::new(()),
        };
        let painted = current.clone(); // 同值更新按API合同复用同一个Arc。
        assert!(current.matches(&painted));
        assert!(current.same_layout_source(&painted));
        let mut navigated = current.clone();
        navigated.preview = Some(Arc::new(()));
        assert!(!navigated.matches(&painted));
        assert!(navigated.same_layout_source(&painted)); // 原连续方向键合同不收紧。
        let next_paint = navigated.clone();
        assert!(navigated.matches(&next_paint));
    }

    #[test]
    fn sensitive_layout_stamp_rejects_zoom_only_without_moving_offset_or_dpi() {
        let current = SensitiveStamp {
            source: source(),
            preview: Some(Arc::new(())),
            offset: point(px(0.), px(0.)),
            viewport: [320., 200.],
            scale: 1.,
            projection: crate::ocr_projection::OcrProjection::new(
                crate::viewport::Viewport::default(),
                1.,
                [320., 200.],
            )
            .unwrap(),
            projection_epoch: Arc::new(()),
            theme: Arc::new(()),
        };
        let mut zoomed = current.clone();
        zoomed.projection = crate::ocr_projection::OcrProjection::new(
            crate::viewport::Viewport {
                zoom: 2.,
                ..Default::default()
            },
            1.,
            [320., 200.],
        )
        .unwrap();
        assert_eq!(zoomed.offset, current.offset);
        assert_eq!(zoomed.viewport, current.viewport);
        assert_eq!(zoomed.scale, current.scale);
        assert!(!current.same_layout_source(&zoomed));
        assert!(!current.matches(&zoomed));
    }

    fn sensitive_paint_fixture() -> (OcrOverlay, SensitiveStamp) {
        let mut source = source();
        let text = "user@example.com";
        let rect = Rect {
            x: 4,
            y: 5,
            width: 160,
            height: 20,
        };
        let geometry = crate::recognition::TextGeometry {
            quad: [
                Point { x: 4., y: 5. },
                Point { x: 164., y: 5. },
                Point { x: 164., y: 25. },
                Point { x: 4., y: 25. },
            ],
        };
        let direction = crate::recognition::HorizontalDirection::LeftToRight;
        let recognized = RecognizedText {
            text: text.into(),
            regions: vec![rect],
            words: vec![crate::recognition::RecognizedWord {
                text: text.into(),
                rect,
                line: 0,
                horizontal_direction: direction,
                geometry: Some(geometry),
            }],
            lines: vec![crate::recognition::RecognizedLine {
                text: text.into(),
                rect: Some(rect),
                horizontal_direction: direction,
                geometry: Some(geometry),
                graphemes: None,
            }],
        };
        let mut overlay = OcrOverlay::new(&recognized, source.target.clone()).unwrap();
        source.overlay = Arc::clone(&overlay.sensitive.identity);
        overlay.sensitive.installation = Some(source.clone());
        overlay.sensitive.preview = Some(Preview::new(
            source.clone(),
            overlay.sensitive.snapshot.as_ref().unwrap(),
        ));
        let stamp = SensitiveStamp {
            source,
            preview: overlay
                .sensitive
                .preview
                .as_ref()
                .map(|preview| Arc::clone(preview.identity())),
            offset: point(px(0.), px(0.)),
            viewport: [320., 200.],
            scale: 1.,
            projection: crate::ocr_projection::OcrProjection::new(
                crate::viewport::Viewport::default(),
                1.,
                [320., 200.],
            )
            .unwrap(),
            projection_epoch: Arc::clone(&overlay.projection_epoch),
            theme: Arc::new(()),
        };
        (overlay, stamp)
    }

    #[test]
    fn sensitive_old_paint_epoch_aba_preserves_current_paint() {
        let (mut overlay, initial) = sensitive_paint_fixture();
        let old = SensitivePaintReceipt::capture(&overlay.sensitive, initial.clone());
        overlay.invalidate_projection();
        overlay.invalidate_projection();
        let mut current = initial.clone();
        current.projection_epoch = Arc::clone(&overlay.projection_epoch);
        assert_eq!(current.projection, initial.projection);
        assert!(!initial.matches(&current));
        let fresh = SensitivePaintReceipt::capture(&overlay.sensitive, current.clone());
        assert!(fresh.publish_current(
            &overlay.sensitive,
            &current,
            &overlay.projection_epoch,
            true,
        ));
        // 持有当前键盘槽的借用，旧paint连读／写该槽都不应发生。
        let keyboard_guard = overlay.sensitive.keyboard.borrow_mut();
        for accessible in [true, false] {
            assert!(!old.publish_current(
                &overlay.sensitive,
                &current,
                &overlay.projection_epoch,
                accessible,
            ));
        }
        drop(keyboard_guard);
        assert!(overlay
            .sensitive
            .painted
            .borrow()
            .as_ref()
            .is_some_and(|painted| painted.matches(&current)));
    }

    #[test]
    fn sensitive_old_preview_theme_or_body_paint_is_neutral() {
        for field in 0..3 {
            let (mut overlay, initial) = sensitive_paint_fixture();
            let old = SensitivePaintReceipt::capture(&overlay.sensitive, initial.clone());
            let mut current = initial.clone();
            match field {
                0 => {
                    let preview = overlay.sensitive.preview.as_mut().unwrap();
                    assert!(preview.set_selected(0, true));
                    current.preview = Some(Arc::clone(preview.identity()));
                }
                1 => current.theme = Arc::new(()),
                _ => current.source.body = Arc::new(()),
            }
            let fresh = SensitivePaintReceipt::capture(&overlay.sensitive, current.clone());
            assert!(fresh.publish_current(
                &overlay.sensitive,
                &current,
                &overlay.projection_epoch,
                true,
            ));
            let keyboard_guard = overlay.sensitive.keyboard.borrow_mut();
            assert!(!old.publish_current(
                &overlay.sensitive,
                &current,
                &overlay.projection_epoch,
                false,
            ));
            drop(keyboard_guard);
            assert!(overlay
                .sensitive
                .painted
                .borrow()
                .as_ref()
                .is_some_and(|painted| painted.matches(&current)));
        }
    }

    #[test]
    fn sensitive_old_paint_replaced_receipt_cells_is_neutral() {
        for replace_keyboard in [false, true] {
            let (mut overlay, current) = sensitive_paint_fixture();
            let old = SensitivePaintReceipt::capture(&overlay.sensitive, current.clone());
            if replace_keyboard {
                overlay.sensitive.keyboard = Default::default();
            } else {
                overlay.sensitive.painted = Default::default();
            }
            let fresh = SensitivePaintReceipt::capture(&overlay.sensitive, current.clone());
            assert!(fresh.publish_current(
                &overlay.sensitive,
                &current,
                &overlay.projection_epoch,
                true,
            ));
            let keyboard_guard = overlay.sensitive.keyboard.borrow_mut();
            assert!(!old.publish_current(
                &overlay.sensitive,
                &current,
                &overlay.projection_epoch,
                false,
            ));
            drop(keyboard_guard);
            assert!(overlay
                .sensitive
                .painted
                .borrow()
                .as_ref()
                .is_some_and(|painted| painted.matches(&current)));
        }
    }

    #[test]
    fn sensitive_current_paint_revokes_only_its_current_unavailable_receipt() {
        let (overlay, current) = sensitive_paint_fixture();
        let receipt = SensitivePaintReceipt::capture(&overlay.sensitive, current.clone());
        assert!(receipt.publish_current(
            &overlay.sensitive,
            &current,
            &overlay.projection_epoch,
            true,
        ));
        assert!(overlay.sensitive.painted.borrow().is_some());
        assert!(!receipt.publish_current(
            &overlay.sensitive,
            &current,
            &overlay.projection_epoch,
            false,
        ));
        assert!(overlay.sensitive.painted.borrow().is_none());
        assert!(overlay.sensitive.preview.is_some());
        assert!(receipt.publish_current(
            &overlay.sensitive,
            &current,
            &overlay.projection_epoch,
            true,
        ));
    }

    fn sensitive_pointer_down() -> MouseDownEvent {
        MouseDownEvent {
            button: MouseButton::Left,
            position: point(px(14.5), px(25.25)),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }
    }

    fn sensitive_pointer_click(down: MouseDownEvent, button: MouseButton) -> gpui::ClickEvent {
        gpui::ClickEvent::Mouse(gpui::MouseClickEvent {
            down,
            up: gpui::MouseUpEvent {
                button,
                position: point(px(14.5), px(25.25)),
                modifiers: gpui::Modifiers::default(),
                click_count: 1,
            },
        })
    }

    #[test]
    fn sensitive_actual_mouse_click_maps_and_consumes_wrong_pairs_or_down_proofs() {
        let original = sensitive_pointer_down();
        for field in 0..4 {
            let mut actual_down = original.clone();
            let mut up_button = MouseButton::Left;
            match field {
                0 => up_button = MouseButton::Right,
                1 => actual_down.button = MouseButton::Right,
                2 => actual_down.position.x += px(1.),
                _ => actual_down.modifiers.shift = true,
            }
            let mut slot = ClickSlot::default();
            slot.arm(
                Arc::new(()),
                "ocr-sensitive",
                pointer_down_proof(&original),
                SensitiveIntent::Apply(Tool::Mosaic),
            );
            let event = sensitive_pointer_click(actual_down, up_button);
            let (actual, left_pair) = pointer_click_proof(&event);
            assert!(slot
                .claim("ocr-sensitive", actual, left_pair, |saved| {
                    *saved == SensitiveIntent::Apply(Tool::Mosaic)
                })
                .is_none());
            assert!(slot.payload().is_none());
        }
    }

    #[test]
    fn sensitive_actual_pointer_slot_keeps_intent_and_rejects_first_mouse_or_keyboard_click() {
        let down = sensitive_pointer_down();
        let mut slot = ClickSlot::default();
        let mut first_mouse = down.clone();
        first_mouse.first_mouse = true;
        slot.arm(
            Arc::new(()),
            "ocr-sensitive",
            pointer_down_proof(&first_mouse),
            SensitiveIntent::Close,
        );
        assert!(slot.payload().is_none());
        slot.arm(
            Arc::new(()),
            "ocr-sensitive",
            pointer_down_proof(&down),
            SensitiveIntent::Toggle(3),
        );
        let (actual, left_pair) = pointer_click_proof(&gpui::ClickEvent::default());
        assert!(slot
            .claim("ocr-sensitive", actual, left_pair, |_| true)
            .is_none());
        assert!(slot.payload().is_none());
        let event = sensitive_pointer_click(down.clone(), MouseButton::Left);
        let (actual, left_pair) = pointer_click_proof(&event);
        slot.arm(
            Arc::new(()),
            "ocr-sensitive",
            pointer_down_proof(&down),
            SensitiveIntent::Toggle(3),
        );
        assert!(slot
            .claim("ocr-sensitive", actual, left_pair, |saved| {
                *saved == SensitiveIntent::Toggle(4)
            })
            .is_none());
        assert!(slot.payload().is_none());
        slot.arm(
            Arc::new(()),
            "ocr-sensitive",
            pointer_down_proof(&down),
            SensitiveIntent::Toggle(3),
        );
        assert!(matches!(
            slot.claim("ocr-sensitive", actual, left_pair, |_| true),
            Some(SensitiveIntent::Toggle(3))
        ));
        assert!(slot
            .claim("ocr-sensitive", actual, left_pair, |_| true)
            .is_none());
    }

    #[test]
    fn sensitive_old_pointer_subscription_token_cannot_clear_new_down() {
        let down = sensitive_pointer_down();
        let old_token = Arc::new(());
        let current_token = Arc::new(());
        let mut slot = ClickSlot::default();
        slot.arm(
            Arc::clone(&old_token),
            "ocr-sensitive",
            pointer_down_proof(&down),
            SensitiveIntent::Open,
        );
        slot.arm(
            Arc::clone(&current_token),
            "ocr-sensitive",
            pointer_down_proof(&down),
            SensitiveIntent::Apply(Tool::Blur),
        );
        slot.cancel_token(&old_token);
        assert!(matches!(
            slot.payload(),
            Some(SensitiveIntent::Apply(Tool::Blur))
        ));
        slot.cancel_token(&current_token);
        assert!(slot.payload().is_none());
    }
}
