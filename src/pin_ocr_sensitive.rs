//! 贴图 OCR 的显式敏感候选；分类只在打开事件，Chrome 不进入 Scene 或历史。
use super::*;
use crate::{
    annotation::{Bounds as Region, Point, Tool},
    geometry::Rect,
    ocr_sensitive::{Preview, Snapshot},
    ocr_sensitive_effect::SensitiveEffectEdit,
};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone)]
struct Source {
    stamp: TextSelectionStamp,
    generation: u64,
    body: Arc<crate::recognition::RecognizedText>,
    roi: Rect,
    image_size: (u32, u32),
    processing: u64,
    theme: Arc<()>,
}
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.stamp == other.stamp
            && self.generation == other.generation
            && Arc::ptr_eq(&self.body, &other.body)
            && self.roi == other.roi
            && self.image_size == other.image_size
            && self.processing == other.processing
            && Arc::ptr_eq(&self.theme, &other.theme)
    }
}

struct Installation {
    snapshot: Snapshot,
    body: Arc<crate::recognition::RecognizedText>,
    stamp: TextSelectionStamp,
    generation: u64,
    roi: Rect,
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Open,
    Toggle(usize),
    Apply(Tool),
    Close,
}

#[derive(Clone)]
struct Stamp {
    source: Source,
    preview: Option<Arc<()>>,
    viewport: crate::viewport::Viewport,
    clip: Region,
    dpi: f32,
    host: (f32, f32),
}
impl Stamp {
    fn matches(&self, other: &Self) -> bool {
        self.source == other.source
            && match (&self.preview, &other.preview) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
            && self.viewport == other.viewport
            && self.clip == other.clip
            && self.dpi == other.dpi
            && self.host == other.host
    }
}

struct Click {
    stamp: Stamp,
    action: Action,
    focus: FocusHandle,
    _blur: gpui::Subscription,
    _activation: gpui::Subscription,
}
#[derive(Default)]
struct ClickState {
    intent: find_ui::FindClickIntent<Click>,
    painted: Option<Stamp>,
}
#[derive(Default)]
pub(super) struct SensitiveState {
    installation: Option<Installation>,
    preview: Option<Preview<Source>>,
    preview_source: Option<Source>,
    click: Rc<RefCell<ClickState>>,
}

/// 所有区域必须完整处于真实 OCR 输出内；不裁掉越界部分后继续遮盖。
fn document_regions(regions: &[Region], roi: Rect, image: (u32, u32)) -> Option<Vec<Region>> {
    if !roi.valid(image.0, image.1) || regions.len() > 4096 {
        return None;
    }
    let mut result = Vec::with_capacity(regions.len());
    for region in regions {
        if !region.x.is_finite()
            || !region.y.is_finite()
            || !region.width.is_finite()
            || !region.height.is_finite()
            || region.x < 0.
            || region.y < 0.
            || region.width <= 0.
            || region.height <= 0.
            || !(region.x + region.width).is_finite()
            || !(region.y + region.height).is_finite()
            || region.x + region.width > roi.width as f32
            || region.y + region.height > roi.height as f32
        {
            return None;
        }
        result.push(Region {
            x: region.x + roi.x as f32,
            y: region.y + roi.y as f32,
            ..*region
        });
    }
    Some(result)
}

fn created_source_matches(before: &Source, after: &Source) -> bool {
    before.stamp.1.is_none()
        && before.stamp.2.is_none()
        && before.stamp.3.is_none()
        && after.stamp.1.is_some()
        && after.stamp.2 == Some(0)
        && after.stamp.3 == Some(Rect::full(before.image_size.0, before.image_size.1))
        && before.stamp.0 == after.stamp.0
        && before.stamp.4 == after.stamp.4
        && before.generation == after.generation
        && Arc::ptr_eq(&before.body, &after.body)
        && before.roi == after.roi
        && before.image_size == after.image_size
        && before.processing == after.processing
        && Arc::ptr_eq(&before.theme, &after.theme)
}

impl PinView {
    pub(super) fn install_pin_sensitive(
        &mut self,
        body: &Arc<crate::recognition::RecognizedText>,
        roi: Rect,
        cx: &App,
    ) {
        self.ocr_sensitive = SensitiveState::default();
        if self.text_source != ocr_clipboard::TextSource::Ocr
            || self.copy_layout_generation == u64::MAX
            || !roi.valid(self.image_size.0, self.image_size.1)
        {
            return;
        }
        let Some(snapshot) = Snapshot::from_recognized(body) else {
            return;
        };
        self.ocr_sensitive.installation = Some(Installation {
            snapshot,
            body: body.clone(),
            stamp: self.text_selection_source(cx),
            generation: self.copy_layout_generation,
            roi,
        });
    }

    fn pin_sensitive_source(&self, cx: &App) -> Option<Source> {
        let installation = self.ocr_sensitive.installation.as_ref()?;
        let current = self.pin_sensitive_host_source(cx)?;
        if self.text_source != ocr_clipboard::TextSource::Ocr
            || !Arc::ptr_eq(&current.body, &installation.body)
            || self.copy_layout_generation != installation.generation
            || current.stamp != installation.stamp
            || self.text_selection_stamp != Some(current.stamp)
            || !installation.roi.valid(self.image_size.0, self.image_size.1)
        {
            return None;
        }
        Some(current)
    }

    // 只供首次创建空画布的局部终核，不授予已安装候选通用重绑定资格。
    fn pin_sensitive_host_source(&self, cx: &App) -> Option<Source> {
        Some(Source {
            stamp: self.text_selection_source(cx),
            generation: self.copy_layout_generation,
            body: self.text.as_ref()?.clone(),
            roi: self.ocr_sensitive.installation.as_ref()?.roi,
            image_size: self.image_size,
            processing: self.processing_request,
            theme: crate::theme::change_identity(cx)?,
        })
    }

    fn pin_sensitive_body_current(&self, source: &Source) -> bool {
        // 仅真实事件核正文，绘制只核不可变安装身份，不逐帧扫描全文。
        self.text_selection
            .as_ref()
            .is_some_and(|selection| selection.text == source.body.text)
            && self
                .ocr_sensitive
                .installation
                .as_ref()
                .is_some_and(|installed| installed.snapshot.text() == source.body.text)
    }

    fn pin_sensitive_input_idle(&self, w: &Window, cx: &App, down: bool) -> bool {
        self.pin_sensitive_interaction_idle(w, cx, down) && (down || self.pin_find_idle(cx))
    }

    fn pin_sensitive_interaction_idle(&self, w: &Window, cx: &App, down: bool) -> bool {
        let pointer = if down {
            (unsafe { GetCapture() == hwnd(w) }) && find_ui::primary_button_down()
        } else {
            unsafe { GetCapture() }.0.is_null() && !find_ui::primary_button_down()
        };
        self.text_mode
            && self.require_ocr_copy_idle(cx).is_ok()
            && !self.presentation_geometry_adjusting
            && self.pending_placement.is_none()
            && self.text_capture.is_none()
            && !self.context_menu_press
            && !self.color_context_menu_open(cx)
            && self.annotation_preview.visible()
            && self.ocr_find.as_ref().is_none_or(|ui| !ui.composing(cx))
            && w.is_window_active()
            && (self.focus.is_focused(w)
                || self.ocr_find_input_focused(w, cx)
                || self.pin_local_candidate_focused(w))
            && pointer
    }

    fn pin_sensitive_clip(&self, w: &Window) -> Region {
        let a = self.text_point(point(px(0.), px(0.)), w);
        let b = self.text_point(point(w.viewport_size().width, w.viewport_size().height), w);
        let x = a.x.max(0.);
        let y = a.y.max(0.);
        Region {
            x,
            y,
            width: (b.x.min(self.image_size.0 as f32) - x).max(0.),
            height: (b.y.min(self.image_size.1 as f32) - y).max(0.),
        }
    }

    fn pin_sensitive_stamp(&self, w: &Window, cx: &App) -> Option<Stamp> {
        let viewport = self.content_viewport(w);
        let dpi = w.scale_factor();
        let clip = self.pin_sensitive_clip(w);
        if !dpi.is_finite()
            || dpi <= 0.
            || !viewport.zoom.is_finite()
            || viewport.zoom <= 0.
            || !viewport.offset.x.is_finite()
            || !viewport.offset.y.is_finite()
            || [clip.x, clip.y, clip.width, clip.height]
                .iter()
                .any(|v| !v.is_finite())
            || clip.width <= 0.
            || clip.height <= 0.
        {
            return None;
        }
        Some(Stamp {
            source: self.pin_sensitive_source(cx)?,
            preview: self
                .ocr_sensitive
                .preview
                .as_ref()
                .map(|p| p.identity().clone()),
            viewport,
            clip,
            dpi,
            host: (
                f32::from(w.viewport_size().width),
                f32::from(w.viewport_size().height),
            ),
        })
    }

    pub(super) fn cancel_pin_sensitive_click(&self) {
        let mut click = self.ocr_sensitive.click.borrow_mut();
        click.intent.cancel();
        click.painted = None;
    }

    pub(super) fn close_pin_sensitive(&mut self) {
        self.cancel_pin_sensitive_click();
        self.ocr_sensitive.preview = None;
        self.ocr_sensitive.preview_source = None;
    }

    pub(super) fn pin_sensitive_preview_open(&self) -> bool {
        self.ocr_sensitive.preview.is_some()
    }

    pub(super) fn validate_pin_sensitive(&mut self, w: &Window, cx: &App) {
        // 查找持有实际输入焦点时，关闭同位置的候选面板；安装保留供再次打开。
        if self.ocr_find.is_some() {
            self.close_pin_sensitive();
        }
        let current = self.pin_sensitive_source(cx);
        let valid = !self.processing
            && !self.processing_image_unavailable
            && self.text_mode
            && current.as_ref().is_some_and(|source| {
                self.ocr_sensitive.preview.is_none()
                    || self.ocr_sensitive.preview_source.as_ref() == Some(source)
            });
        if !valid {
            self.close_pin_sensitive();
        }
        let state = self.ocr_sensitive.click.borrow();
        let click_current = state.intent.payload().is_none_or(|click| {
            self.pin_sensitive_click_current(click, w, cx, state.intent.is_armed())
        });
        drop(state);
        if !click_current {
            self.cancel_pin_sensitive_click();
        }
    }

    fn pin_sensitive_click_current(&self, click: &Click, w: &Window, cx: &App, down: bool) -> bool {
        self.pin_sensitive_input_idle(w, cx, down)
            && click.focus.is_focused(w)
            && self
                .pin_sensitive_stamp(w, cx)
                .is_some_and(|current| click.stamp.matches(&current))
            && (matches!(
                click.action,
                Action::Open | Action::Close | Action::Toggle(_)
            ) || !self.state.locked)
    }

    fn pin_sensitive_mouse(
        &mut self,
        phase: find_ui::FindClickPhase,
        stamp: &Stamp,
        action: Action,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match phase {
            find_ui::FindClickPhase::Down => {
                self.ocr_sensitive.click.borrow_mut().intent.cancel();
                if !self.pin_sensitive_input_idle(w, cx, true)
                    || !self
                        .pin_sensitive_stamp(w, cx)
                        .is_some_and(|current| stamp.matches(&current))
                    || !self
                        .ocr_sensitive
                        .click
                        .borrow()
                        .painted
                        .as_ref()
                        .is_some_and(|painted| stamp.matches(painted))
                    || !self.pin_sensitive_body_current(&stamp.source)
                    || matches!(action, Action::Apply(_)) && self.state.locked
                {
                    return;
                }
                let Some(focus) = w.focused(cx) else { return };
                let token = Arc::new(());
                let blur_token = token.clone();
                let blur = cx.on_blur(&focus, w, move |view, _, cx| {
                    if view
                        .ocr_sensitive
                        .click
                        .borrow()
                        .intent
                        .token()
                        .is_some_and(|token| Arc::ptr_eq(token, &blur_token))
                    {
                        view.cancel_pin_sensitive_click();
                        cx.notify();
                    }
                });
                let activation_token = token.clone();
                let activation = cx.observe_window_activation(w, move |view, w, cx| {
                    if !w.is_window_active()
                        && view
                            .ocr_sensitive
                            .click
                            .borrow()
                            .intent
                            .token()
                            .is_some_and(|token| Arc::ptr_eq(token, &activation_token))
                    {
                        view.cancel_pin_sensitive_click();
                        cx.notify();
                    }
                });
                self.ocr_sensitive.click.borrow_mut().intent.arm(
                    token,
                    Click {
                        stamp: stamp.clone(),
                        action,
                        focus,
                        _blur: blur,
                        _activation: activation,
                    },
                );
            }
            find_ui::FindClickPhase::Up => {
                let state = self.ocr_sensitive.click.borrow();
                let current = state.intent.is_armed()
                    && state.intent.payload().is_some_and(|click| {
                        click.action == action
                            && click.stamp.matches(stamp)
                            && self.pin_sensitive_click_current(click, w, cx, false)
                            && self.pin_sensitive_body_current(&click.stamp.source)
                    });
                drop(state);
                if current {
                    self.ocr_sensitive.click.borrow_mut().intent.queue();
                } else {
                    self.cancel_pin_sensitive_click();
                }
            }
        }
        cx.notify();
    }

    pub(super) fn discard_pin_sensitive_release(&self) {
        // 新 Down 撤销旧点击，保留该张实际 paint 可登记自己的 Down。
        self.ocr_sensitive.click.borrow_mut().intent.cancel();
    }

    pub(super) fn schedule_pin_sensitive_release_cleanup(
        &self,
        w: &mut Window,
        cx: &Context<Self>,
    ) {
        let state = self.ocr_sensitive.click.borrow();
        if !state.intent.is_armed() {
            return;
        }
        let Some(token) = state.intent.token().cloned() else {
            return;
        };
        drop(state);
        let weak = cx.entity().downgrade();
        w.on_next_frame(move |_, cx| {
            let _ = weak.update(cx, |view, cx| {
                let current = {
                    let state = view.ocr_sensitive.click.borrow();
                    state.intent.is_armed()
                        && state.intent.token().is_some_and(|t| Arc::ptr_eq(t, &token))
                };
                if current {
                    view.ocr_sensitive
                        .click
                        .borrow_mut()
                        .intent
                        .release_outside(&token);
                    view.cancel_pin_sensitive_click();
                    cx.notify();
                }
            });
        });
    }

    fn run_pin_sensitive_click(
        &mut self,
        ticket: &find_ui::FindPaintTicket,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let click = self.ocr_sensitive.click.borrow_mut().intent.take(ticket);
        let Some(click) = click else { return };
        if !self.pin_sensitive_click_current(&click, w, cx, false)
            || !self.pin_sensitive_body_current(&click.stamp.source)
            || !self
                .ocr_sensitive
                .click
                .borrow()
                .painted
                .as_ref()
                .is_some_and(|p| click.stamp.matches(p))
        {
            return;
        }
        match click.action {
            Action::Open => {
                let installation = self
                    .ocr_sensitive
                    .installation
                    .as_ref()
                    .expect("敏感候选安装已核验");
                let preview_source = click.stamp.source;
                let preview = Preview::new(preview_source.clone(), &installation.snapshot);
                self.close_pin_sensitive();
                self.cancel_pin_find_click();
                self.ocr_find = None;
                self.cancel_pin_local_intent("打开敏感文字候选");
                self.ocr_sensitive.preview = Some(preview);
                self.ocr_sensitive.preview_source = Some(preview_source);
                w.focus(&self.focus);
            }
            Action::Close => self.close_pin_sensitive(),
            Action::Toggle(id) => {
                if let Some(preview) = &mut self.ocr_sensitive.preview {
                    preview.set_selected(id, !preview.selected(id));
                }
                self.cancel_pin_sensitive_click();
            }
            Action::Apply(tool) => {
                if let Err(error) = self.apply_pin_sensitive(&click.stamp, tool, w, cx) {
                    self.status = trf!(
                        "敏感文字处理失败：{error}；可取消或重试",
                        "Sensitive text failed: {error}; cancel or retry"
                    );
                }
            }
        }
        cx.notify();
    }

    fn apply_pin_sensitive(
        &mut self,
        stamp: &Stamp,
        tool: Tool,
        w: &Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        let source = self.pin_sensitive_source(cx).ok_or("敏感文字来源已改变")?;
        if !stamp.source.eq(&source)
            || !self.pin_sensitive_input_idle(w, cx, false)
            || self.state.locked
            || !self.pin_sensitive_body_current(&source)
        {
            return Err("敏感文字确认已失效".into());
        }
        let preview = self
            .ocr_sensitive
            .preview
            .as_ref()
            .ok_or("敏感候选已关闭")?;
        let identity = stamp.preview.as_ref().ok_or("敏感候选身份缺失")?.clone();
        let local = preview
            .selected_regions(&source, &source.body.text, &identity)
            .ok_or("敏感候选选择已改变")?;
        let regions = document_regions(&local, source.roi, source.image_size)
            .ok_or("敏感候选几何越界或无效")?;
        if regions.is_empty() {
            return Ok(());
        }
        let style = crate::tool_presets::default_style(tool);
        let scene = if let Some(canvas) = &self.canvas {
            canvas.update(cx, |view, _| -> Result<crate::scene::Scene> {
                let edit = SensitiveEffectEdit::prepare(
                    &view.editor.document,
                    source.clone(),
                    identity.clone(),
                    regions.clone(),
                    tool,
                    style,
                )?
                .ok_or("敏感候选为空")?;
                let annotations = edit
                    .annotations(&view.editor.document)
                    .ok_or("贴图文档已改变")?;
                let prepared = view.prepared_scene();
                Ok(prepared.updated(&annotations, &view.source_pixels()))
            })?
        } else {
            let full = Rect::full(self.image_size.0, self.image_size.1);
            let fill = self.vector_text_fill();
            let source_pixels = crate::scene::SourcePixels {
                fill,
                canvas: full,
                planes: if fill.is_some() {
                    Vec::new()
                } else {
                    vec![crate::scene::PixelPlane {
                        rect: full,
                        bgra: self.image.as_bytes(0).ok_or("贴图底图没有像素")?,
                    }]
                },
                overlay: None,
            };
            let document = crate::document::Document::default();
            let edit = SensitiveEffectEdit::prepare(
                &document,
                source.clone(),
                identity.clone(),
                regions.clone(),
                tool,
                style,
            )?
            .ok_or("敏感候选为空")?;
            let annotations = edit.annotations(&document).ok_or("贴图文档已改变")?;
            crate::scene::Scene::default().updated(&annotations, &source_pixels)
        };
        if let Some(error) = scene.first_error() {
            return Err(error.into());
        }
        // Scene 准备成功后才创建文档；失败不改变 OCR、历史或原图。
        if !self
            .pin_sensitive_stamp(w, cx)
            .is_some_and(|current| stamp.matches(&current))
            || !self.pin_sensitive_input_idle(w, cx, false)
            || !self.pin_sensitive_body_current(&source)
        {
            return Err("敏感文字来源已改变".into());
        }
        let created = self.canvas.is_none();
        self.ensure_document_canvas(w, cx)?;
        let canvas = self.canvas.as_ref().expect("敏感文字文档已创建").clone();
        // 新画布只允许本调用的 None→空文档；不得把任意更新来源重绑给旧候选。
        let Some(committed_source) = self.pin_sensitive_host_source(cx) else {
            if created {
                self.canvas = None;
                self.canvas_subscription = None;
            }
            return Err("敏感文字来源已改变".into());
        };
        let source_current = if created {
            created_source_matches(&source, &committed_source)
                && canvas.read(cx).editor.document.items().is_empty()
                && canvas.read(cx).editor.document.crop_mask.is_none()
        } else {
            source == committed_source
        };
        let authorized = source_current
            && self.pin_sensitive_interaction_idle(w, cx, false)
            && self.pin_sensitive_body_current(&source)
            && self
                .ocr_sensitive
                .preview
                .as_ref()
                .is_some_and(|p| p.current(&source, &source.body.text, &identity))
            && crate::theme::change_identity(cx).is_some_and(|t| Arc::ptr_eq(&source.theme, &t));
        if !authorized {
            if created {
                self.canvas = None;
                self.canvas_subscription = None;
            }
            return Err("敏感文字来源已改变".into());
        }
        let committed = canvas.update(cx, |view, cx| -> Result<bool> {
            // 此计划绑定真正已创建的 Document；Scene 已用相同对象和真实像素验完。
            let edit = SensitiveEffectEdit::prepare(
                &view.editor.document,
                committed_source.clone(),
                identity.clone(),
                regions,
                tool,
                style,
            )?
            .ok_or("敏感候选为空")?;
            let final_scene = if created {
                let annotations = edit
                    .annotations(&view.editor.document)
                    .ok_or("贴图文档已改变")?;
                view.prepared_scene()
                    .updated(&annotations, &view.source_pixels())
            } else {
                scene
            };
            if let Some(error) = final_scene.first_error() {
                return Err(error.into());
            }
            if !edit.commit(
                &mut view.editor.document,
                &committed_source,
                &identity,
                authorized,
            ) {
                return Ok(false);
            }
            view.scene = Arc::new(final_scene);
            view.scene_stamp = None;
            cx.notify();
            Ok(true)
        });
        if !matches!(committed, Ok(true)) {
            if created {
                self.canvas = None;
                self.canvas_subscription = None;
            }
            return committed.and_then(|_| Err("敏感文字文档已改变".into()));
        }
        self.redo_edits.clear();
        self.invalidate_text_overlay();
        self.status = tr!(
            "敏感文字已处理；Ctrl+Z 撤销整批",
            "Sensitive text applied; Ctrl+Z undoes the batch"
        )
        .into();
        Ok(())
    }

    pub(super) fn pin_sensitive_escape(
        &mut self,
        e: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if e.keystroke.key != "escape"
            || self.ocr_sensitive.preview.is_none()
            || self.context_menu_press
            || self.color_context_menu_open(cx)
        {
            return false;
        }
        self.close_pin_sensitive();
        cx.notify();
        true
    }

    fn sensitive_button(
        &self,
        button: gpui::Stateful<gpui::Div>,
        stamp: &Stamp,
        action: Action,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let down = stamp.clone();
        let up = stamp.clone();
        button
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, _, w, cx| {
                    v.pin_sensitive_mouse(find_ui::FindClickPhase::Down, &down, action, w, cx);
                    cx.stop_propagation();
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |v, _, w, cx| {
                    v.pin_sensitive_mouse(find_ui::FindClickPhase::Up, &up, action, w, cx);
                    cx.stop_propagation();
                }),
            )
            .on_click(|_, _, cx| cx.stop_propagation())
    }

    pub(super) fn render_pin_sensitive(
        &self,
        w: &Window,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let stamp = self.pin_sensitive_stamp(w, cx)?;
        if !self.text_mode || self.processing || self.processing_image_unavailable {
            return None;
        }
        let state = self.ocr_sensitive.click.clone();
        let epoch = state.borrow_mut().intent.begin_render();
        let release = {
            let state = state.borrow();
            (!state.intent.is_armed())
                .then(|| state.intent.token().cloned())
                .flatten()
        };
        let owner = cx.entity().downgrade();
        let painted_stamp = stamp.clone();
        let palette = crate::theme::palette(cx);
        let quads: Vec<_> = self
            .ocr_sensitive
            .preview
            .as_ref()
            .map_or_else(Vec::new, |preview| {
                preview
                    .candidates()
                    .iter()
                    .flat_map(|candidate| {
                        candidate.quads.iter().map(|quad| {
                            (
                                quad.map(|p| Point {
                                    x: p.x + stamp.source.roi.x as f32,
                                    y: p.y + stamp.source.roi.y as f32,
                                }),
                                preview.selected(candidate.id),
                            )
                        })
                    })
                    .collect()
            });
        let mut root = div().absolute().top_0().left_0().size_full().child(
            canvas(
                |_, _, _| (),
                move |_, _, w, cx| {
                    if !state.borrow().intent.epoch_is_current(&epoch) {
                        return;
                    }
                    let current = owner.upgrade().is_some_and(|owner| {
                        let view = owner.read(cx);
                        view.pin_sensitive_input_idle(w, cx, false)
                            && view
                                .pin_sensitive_stamp(w, cx)
                                .is_some_and(|current| painted_stamp.matches(&current))
                    });
                    let recovered = {
                        let mut state = state.borrow_mut();
                        let recovered = current
                            && !state
                                .painted
                                .as_ref()
                                .is_some_and(|p| p.matches(&painted_stamp));
                        state.painted = current.then(|| painted_stamp.clone());
                        recovered
                    };
                    if recovered {
                        let weak = owner.clone();
                        w.on_next_frame(move |_, cx| {
                            let _ = weak.update(cx, |_, cx| cx.notify());
                        });
                    }
                    let eligible_release = current
                        && owner.upgrade().is_some_and(|owner| {
                            let view = owner.read(cx);
                            let state = state.borrow();
                            !state.intent.is_armed()
                                && release
                                    .as_ref()
                                    .zip(state.intent.token())
                                    .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
                                && state.intent.payload().is_some_and(|click| {
                                    view.pin_sensitive_click_current(click, w, cx, false)
                                })
                        });
                    if eligible_release {
                        if let Some(ticket) = state.borrow_mut().intent.painted(&epoch) {
                            let weak = owner.clone();
                            w.on_next_frame(move |w, cx| {
                                let _ = weak.update(cx, |view, cx| {
                                    view.run_pin_sensitive_click(&ticket, w, cx)
                                });
                            });
                        }
                    }
                    if !current {
                        return;
                    }
                    let viewport = painted_stamp.viewport;
                    let dpi = painted_stamp.dpi;
                    let clip = painted_stamp.clip;
                    w.with_content_mask(
                        Some(gpui::ContentMask {
                            bounds: Bounds::new(
                                point(
                                    px((viewport.offset.x + clip.x * viewport.zoom) / dpi),
                                    px((viewport.offset.y + clip.y * viewport.zoom) / dpi),
                                ),
                                size(
                                    px(clip.width * viewport.zoom / dpi),
                                    px(clip.height * viewport.zoom / dpi),
                                ),
                            ),
                        }),
                        |w| {
                            for (quad, selected) in &quads {
                                let vertices = quad.map(|p| {
                                    point(
                                        px((viewport.offset.x + p.x * viewport.zoom) / dpi),
                                        px((viewport.offset.y + p.y * viewport.zoom) / dpi),
                                    )
                                });
                                let mut fill = gpui::Path::new(vertices[0]);
                                let uv = point(0., 1.);
                                fill.push_triangle(
                                    (vertices[0], vertices[1], vertices[2]),
                                    (uv, uv, uv),
                                );
                                fill.push_triangle(
                                    (vertices[0], vertices[2], vertices[3]),
                                    (uv, uv, uv),
                                );
                                w.paint_path(
                                    fill,
                                    rgba(if *selected { 0x378de55a } else { 0xf2aa2630 }),
                                );
                                let mut border = gpui::PathBuilder::stroke(px(1.5));
                                border.move_to(vertices[0]);
                                for vertex in vertices.iter().skip(1).chain(vertices.first()) {
                                    border.line_to(*vertex);
                                }
                                if let Ok(border) = border.build() {
                                    w.paint_path(border, rgb(palette.foreground));
                                }
                            }
                        },
                    );
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        );
        let width = (stamp.host.0 - 16.).clamp(1., 360.);
        let height = (stamp.host.1 - 16.).clamp(1., 280.);
        if let Some(preview) = &self.ocr_sensitive.preview {
            let mut panel = div()
                .id("pin-sensitive-panel")
                .absolute()
                .top(px(8.))
                .left(px(8.))
                .w(px(width))
                .max_h(px(height))
                .p_2()
                .flex()
                .flex_col()
                .gap(px(6.))
                .rounded_md()
                .shadow_md()
                .border_1()
                .border_color(rgb(palette.border))
                .bg(rgb(palette.background))
                .text_color(rgb(palette.foreground))
                .text_size(px(12.))
                .overflow_y_scroll()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(div().flex_shrink_0().child(tr!(
                    "敏感文字候选 · 逐项勾选",
                    "Sensitive candidates · select items"
                )));
            let mut list = div()
                .id("pin-sensitive-items")
                .flex_shrink_0()
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
                let button = div()
                    .id(("pin-sensitive-item", candidate.id))
                    .w_full()
                    .flex_shrink_0()
                    .p_1()
                    .border_1()
                    .border_color(rgb(palette.border))
                    .rounded_sm()
                    .overflow_hidden()
                    .cursor_pointer()
                    .hover(move |s| s.bg(rgb(palette.hover)))
                    .child(preview.item_label(candidate));
                list = list.child(self.sensitive_button(
                    button,
                    &stamp,
                    Action::Toggle(candidate.id),
                    cx,
                ));
            }
            let selected = preview.selected_count();
            panel = panel.child(list).child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(rgb(palette.muted))
                    .child(trf!(
                        "已选 {selected} 项；取消不会改图",
                        "{selected} selected; cancel keeps image"
                    )),
            );
            let mut controls = div().flex().flex_shrink_0().flex_wrap().gap(px(4.));
            for (id, label, action, enabled) in [
                (
                    "pin-sensitive-mosaic",
                    tr!("确认马赛克", "Apply mosaic"),
                    Action::Apply(Tool::Mosaic),
                    selected > 0 && !self.state.locked,
                ),
                (
                    "pin-sensitive-blur",
                    tr!("确认模糊", "Apply blur"),
                    Action::Apply(Tool::Blur),
                    selected > 0 && !self.state.locked,
                ),
                (
                    "pin-sensitive-close",
                    tr!("取消", "Cancel"),
                    Action::Close,
                    true,
                ),
            ] {
                let button = div()
                    .id(id)
                    .w(px(98_f32.min((width - 16.).max(1.))))
                    .h(px(28.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .bg(rgb(palette.hover))
                    .opacity(if enabled { 1. } else { 0.45 })
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .tooltip(move |_, cx| cx.new(move |_| TextToolHint(label)).into())
                    .child(div().min_w(px(0.)).truncate().child(label));
                controls = controls.child(if enabled {
                    self.sensitive_button(button.cursor_pointer(), &stamp, action, cx)
                } else {
                    button
                });
            }
            root = root.child(panel.child(controls));
        } else {
            let button = div()
                .id("pin-sensitive-open")
                .absolute()
                // 有选字时给最底行的“重识别此行”留出独立位置。
                .bottom(px(if self.has_selected_text() { 44. } else { 8. }))
                .right(px(8.))
                .w(px(110_f32.min((stamp.host.0 - 16.).max(1.))))
                .h(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .bg(rgb(palette.background))
                .border_1()
                .border_color(rgb(palette.border))
                .text_size(px(12.))
                .text_color(rgb(palette.foreground))
                .overflow_hidden()
                .cursor_pointer()
                .tooltip(move |_, cx| {
                    cx.new(move |_| TextToolHint(tr!("敏感文字", "Sensitive text")))
                        .into()
                })
                .child(
                    div()
                        .min_w(px(0.))
                        .truncate()
                        .child(tr!("敏感文字", "Sensitive text")),
                );
            root = root.child(self.sensitive_button(button, &stamp, Action::Open, cx));
        }
        Some(root.into_any_element())
    }
}

#[cfg(test)]
#[path = "pin_ocr_sensitive_tests.rs"]
mod tests;
