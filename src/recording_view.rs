//! 录屏宿主：选区边框 + 排除采集的传输工具栏 + 倒计时 + 停止后的框内预览。
//! 状态：选区待录 → (倒计时) → 录制/暂停 → 完成编码/GIF 转换 → 框内预览 → 重录或关闭。
use crate::recording_key_owner as key_owner;
#[path = "recording_audio_gain.rs"]
mod audio_gain;
#[path = "recording_audio_input.rs"]
mod audio_input;
#[path = "recording_device_choices.rs"]
mod device_choices;
#[path = "recording_annotation_view.rs"]
mod drawing_host;
#[path = "recording_encoding_feedback.rs"]
mod encoding_feedback;
#[path = "recording_encoding_info.rs"]
mod encoding_info;
#[path = "recording_output_state.rs"]
mod output_state;
#[path = "recording_quality_draft.rs"]
mod quality_draft;
#[path = "recording_quality_editor.rs"]
mod quality_editor;
#[path = "recording_shortcuts.rs"]
mod shortcut_keys;
#[path = "recording_size_feedback.rs"]
mod size_feedback;
#[path = "recording_tip.rs"]
mod tip;
#[path = "recording_video_type_feedback.rs"]
mod video_type_feedback;
#[path = "recording_volume_intent.rs"]
mod volume_intent;
#[path = "recording_volume_save.rs"]
pub(crate) mod volume_save;
use quality_editor::QualityEditor;
use recording::live_annotation::{Geometry as DrawingGeometry, LiveTool};

use crate::{
    audio_capture::{AudioInputCheck, AudioSource, CaptureLevel, CaptureSourceState, MixLevels},
    audio_devices::{EndpointKind, EndpointSelection},
    capture_toolbar::Palette,
    geometry::ScreenRect,
    gpui_capture::hwnd,
    long_capture_border::LongCaptureBorder,
    long_region::{RegionGesture, RegionHandle},
    long_region_input::RegionInputEvent,
    pin_model::Placement,
    recording::{self, Phase, Recording, RecordingResult},
    recording_encoder_policy::EncoderPolicy,
    recording_options::{
        format_bytes, format_clock, KeyPosition, Preset, QualityProfile, RecordingOptions,
        DELAY_CHOICES, FPS_CHOICES,
    },
    recording_preview::{RecordingPreview, RATES},
    stitch::ScrollAxis,
    toolbar_icons::{self, Icon},
    Result,
};
use gpui::{
    canvas, div, prelude::*, px, rgb, rgba, App, AppContext, Bounds, Context, FocusHandle,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, Render, SharedString,
    Window, WindowBounds, WindowHandle, WindowOptions,
};
use key_owner::KeyOwnerProbe;
use shortcut_keys::{
    Action as ShortcutAction, Context as ShortcutContext, Modifiers as ShortcutModifiers,
    Route as ShortcutRoute, Stage as ShortcutStage,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{HWND, POINT},
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST},
    UI::WindowsAndMessaging::*,
};

const IDLE_COLOR: u32 = 0x1E90FF;
const RECORDING_COLOR: u32 = 0xE53935;
const PAUSED_COLOR: u32 = 0xF5A623;
const RED: u32 = 0xE53935;
const BAR_HEIGHT: f32 = 40.;
const SEEK_HEIGHT: f32 = 12.;
/// 声音、显示设置、混音这类带说明文字的面板宽度；纯选项下拉按内容收窄。
const FLYOUT_WIDTH: f32 = 244.;
const ANNOTATION_HEIGHT: f32 = 40.;
/// 计时器文字最小宽度：容得下“1:02:03”“编码中…”，开始录制前后工具栏宽度不变。
const TIMER_WIDTH: f32 = 48.;
const INFO_MIN_WIDTH: f32 = 120.;
const RATE_MIN_WIDTH: f32 = 32.;
/// 标注颜色，与截图标注的常用色一致。
const SWATCHES: [u32; 7] = [
    0xff4b5c, 0xffd54f, 0x45bb80, 0x4f9cff, 0xaa77ee, 0xffffff, 0x171717,
];
const SWATCH: f32 = 20.;
const STYLE_WIDTH: f32 = 2. + 12. + SWATCH * 7. + 6. * 6.;
const STYLE_HEIGHT: f32 = 2. + 12. + SWATCH + 8. + 24. + 8. + 24.;
/// 按钮说明：首次悬停多久出现（录制中更久，避免去点暂停时冒出来）、之后多久内移到相邻按钮立即显示。
const TIP_DELAY: Duration = Duration::from_millis(600);
const TIP_DELAY_RECORDING: Duration = Duration::from_millis(1000);
const TIP_WARM: Duration = Duration::from_millis(1500);
const FLASH: Duration = Duration::from_millis(1600);
const FLASH_NOTE: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Idle,
    Countdown,
    Recording,
    Preview,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewPlaybackState {
    Opening,
    Ready,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flyout {
    Delay,
    Fps,
    Preset,
    Quality,
    AdvancedQuality,
    EncodingInfo,
    Audio,
    OutputDevice,
    InputDevice,
    Input,
    Rate,
    Mix,
    /// 标注工具栏里的颜色、粗细、透明度弹层。
    Style,
}
impl Flyout {
    /// 下拉左缘对齐的按钮。
    fn anchor(self) -> &'static str {
        match self {
            Flyout::Delay => "rec-timer",
            Flyout::Fps => "rec-fps",
            Flyout::Preset | Flyout::Quality | Flyout::AdvancedQuality => "rec-preset",
            Flyout::Audio | Flyout::OutputDevice | Flyout::InputDevice => "rec-audio",
            Flyout::Input => "rec-input",
            Flyout::Rate => "preview-rate",
            Flyout::Mix => "preview-mix",
            Flyout::EncodingInfo => "preview-info",
            Flyout::Style => "ann-style",
        }
    }
    /// 面板高度（DIP）。纯选项下拉：上下各 4 内边距加边框，每行 28。
    fn height(self) -> f32 {
        let list = |rows: usize| 10. + 28. * rows as f32;
        match self {
            Flyout::Delay => list(DELAY_CHOICES.len()) + 22.,
            Flyout::Fps => list(FPS_CHOICES.len()),
            Flyout::Preset => {
                list(Preset::ALL.len())
                    + if crate::file_launch_command::experimental_media_enabled() {
                        24. + 28. * EncoderPolicy::ALL.len() as f32 + 50. + 28.
                    } else {
                        0.
                    }
            }
            Flyout::Quality => list(QualityProfile::EXPERIMENTS.len() + 3) + 96.,
            Flyout::AdvancedQuality | Flyout::EncodingInfo => 430.,
            Flyout::Rate => list(RATES.len()),
            Flyout::Audio => {
                28. * 2.
                    + 34.
                    + 14.
                    + if crate::file_launch_command::experimental_media_enabled() {
                        332.
                    } else {
                        0.
                    }
            }
            Flyout::OutputDevice | Flyout::InputDevice => 238.,
            Flyout::Input => 28. * 5. + 54. + 14.,
            Flyout::Mix => 14. + 2. * (20. + 28.) + 36.,
            Flyout::Style => STYLE_HEIGHT,
        }
    }
}

fn delay_label(delay: u32) -> String {
    if delay == 0 {
        tr!("不延迟", "No delay").into()
    } else {
        trf!("{} 秒", "{}s", delay)
    }
}

thread_local! {
    /// 最近一帧各按钮在工具栏窗口内的位置（逻辑像素），说明气泡与下拉据此对齐。
    static BUTTON_BOUNDS: RefCell<HashMap<&'static str, Bounds<Pixels>>> =
        RefCell::new(HashMap::new());
}
fn button_bounds(id: &'static str) -> Option<Bounds<Pixels>> {
    BUTTON_BOUNDS.with(|map| map.borrow().get(id).copied())
}
fn bounds_probe(id: &'static str) -> impl IntoElement {
    canvas(
        move |bounds, _, _| {
            BUTTON_BOUNDS.with(|map| {
                map.borrow_mut().insert(id, bounds);
            });
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// 估算与实测取大：宽度宁可多出几像素（由弹性空白吸收），也不裁掉末尾按钮。
fn label_width(window: Option<&Window>, text: &str, size: f32) -> f32 {
    let estimate = crate::property_items::text_width(text, size);
    let Some(window) = window else {
        return estimate.ceil();
    };
    let line = window.text_system().shape_line(
        text.to_owned().into(),
        px(size),
        &[gpui::TextRun {
            len: text.len(),
            font: window.text_style().font(),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    estimate.max(f32::from(line.width)).ceil() + 1.
}

/// 一行按钮的总宽：左右内边距 6、边框 1、子项间距 2。
#[derive(Default)]
struct RowWidth {
    sum: f32,
    count: u32,
}
impl RowWidth {
    fn add(&mut self, width: f32) {
        self.sum += width;
        self.count += 1;
    }
    fn total(&self) -> f32 {
        self.sum + 2. * self.count.saturating_sub(1) as f32 + 12. + 2.
    }
}

/// 待录／录制工具栏宽度：设置组（计时、帧率、格式、声音、按键显示）+ 录制组（录制、暂停、标注、显示隐藏）+ 关闭。
fn transport_width(window: Option<&Window>, options: &RecordingOptions) -> f32 {
    let chip = |text: &str| 12. + label_width(window, text, 12.);
    let mut row = RowWidth::default();
    row.add(14.);
    row.add(12. + TIMER_WIDTH + 2. + 10.);
    row.add(chip(&format!("{}FPS", options.fps)));
    row.add(chip(options.preset.label()));
    for width in [30., 30., 7., 30., 30., 30., 30., 0., 7., 30.] {
        row.add(width);
    }
    row.total()
}

/// 标注工具栏：5 个工具、样式、撤销、重做、删除、清空、完成，3 条分隔线。
fn annotation_width() -> f32 {
    let mut row = RowWidth::default();
    for width in [
        30., 30., 30., 30., 30., 7., 30., 7., 30., 30., 30., 30., 7., 30.,
    ] {
        row.add(width);
    }
    row.total()
}

/// 悬停变化转给工具栏视图；不可用的按钮按“未悬停”处理，不出说明。
fn set_hover(
    window: &mut Window,
    cx: &mut App,
    id: &'static str,
    hint: SharedString,
    hovered: bool,
) {
    if let Some(Some(view)) = window.root::<RecordingView>() {
        view.update(cx, |v, _| v.hover_changed(id, hint, hovered, window));
    }
}
/// 按下按钮：说明立即收起，移开这个按钮前不再出现；错误消息视为已处理。
fn tip_pressed(window: &mut Window, cx: &mut App, id: &'static str) {
    if let Some(Some(view)) = window.root::<RecordingView>() {
        view.update(cx, |v, _| v.tip_pressed(id));
    }
}

/// 混音任务完成后要继续的操作。
#[derive(Clone, Debug, PartialEq, Eq)]
enum MixAction {
    Save(volume_save::SaveRequest),
    Copy,
    Close,
}
impl MixAction {
    fn kind(&self) -> output_state::OutputKind {
        match self {
            Self::Save(_) => output_state::OutputKind::Save,
            Self::Copy => output_state::OutputKind::Copy,
            Self::Close => output_state::OutputKind::Close,
        }
    }
}

/// 按新音量重新混音的后台任务：视频直通，只重编码音轨。
struct MixJob {
    outcome: Mutex<Option<std::result::Result<volume_save::StagedSave, String>>>,
    cancel: AtomicBool,
    save_intent: Option<volume_intent::SaveIntent>,
    snapshot: output_state::OutputSnapshot,
    request: Option<volume_save::SaveRequest>,
}

const SLIDER_WIDTH: f32 = 160.;

/// 预览中“转为 GIF”的后台任务。
struct GifJob {
    progress: AtomicU32,
    cancel: AtomicBool,
    outcome: Mutex<Option<std::result::Result<volume_save::StagedSave, String>>>,
    snapshot: output_state::OutputSnapshot,
    request: volume_save::SaveRequest,
    intent: volume_intent::SaveIntent,
}

/// 悬停中的按钮；到时显示说明气泡。
struct Hover {
    id: &'static str,
    text: SharedString,
    since: Instant,
    shown: bool,
}

/// 工具栏外侧的消息条：进度 4 秒、警告 6 秒（指针停在上面时暂停），错误留到点掉或下一次操作。
struct Notice {
    tone: tip::Tone,
    text: String,
    until: Option<Instant>,
}

/// 成功就地反馈：按钮图标短暂变成对勾，补充说明放进该按钮的悬停说明。
struct Flash {
    id: &'static str,
    until: Instant,
    note: SharedString,
    note_until: Instant,
}

pub struct RecordingView {
    region: ScreenRect,
    options: RecordingOptions,
    /// 仅实验环境中的本窗口选择，不写入既有录屏配置。
    encoder_policy: EncoderPolicy,
    quality_profile: Option<QualityProfile>,
    quality_editor: QualityEditor,
    /// 只保留最后一次控制状态，便于读取失败会话的收尾快照；不持有 GPU/Reader。
    last_encoding_session: Option<Arc<Recording>>,
    device_choices: device_choices::DeviceChoices,
    audio_input_check: Option<AudioInputCheck>,
    audio_input_stopping: bool,
    audio_input_revision: Option<u64>,
    record_after_input_check: bool,
    stage: Stage,
    recording: Option<Arc<Recording>>,
    /// 倒计时中取消的预备会话；其线程收尾释放桌面复制前不能再开新会话。
    abandoned: Option<Arc<Recording>>,
    result: Option<RecordingResult>,
    /// 预览中当前对外的成品（转 GIF 后切到 GIF）。
    output: Option<PathBuf>,
    session_files: output_state::SessionFiles,
    gif_job: Option<Arc<GifJob>>,
    /// 预览／最终混音的音量；成片初始按默认音量混入。
    levels: MixLevels,
    mix_job: Option<(Arc<MixJob>, MixAction)>,
    save_owner: volume_intent::SaveOwner,
    /// 已原子提交到录屏目录的复制成品；失效只释放引用，不删除成品。
    published_copy: Option<output_state::PublishedCopy>,
    mix_drag: Option<AudioSource>,
    border: Option<LongCaptureBorder>,
    border_color: u32,
    preview: Option<WindowHandle<RecordingPreview>>,
    preview_audio_warning: Option<String>,
    drawing: Option<WindowHandle<drawing_host::DrawingView>>,
    countdown: Option<(Instant, WindowHandle<CountdownView>)>,
    flyout: Option<Flyout>,
    region_drag: Option<(RegionGesture, ScreenRect)>,
    focus: FocusHandle,
    key_owner: Option<KeyOwnerProbe>,
    /// 原生保存窗等待结果期间，键盘归对话框，禁止重复安排输出动作。
    picking_save: bool,
    notice: Option<Notice>,
    flash: Option<Flash>,
    /// 拖动选区边时的尺寸标签；松手后到时隐藏。
    size_label: Option<(ScreenRect, Option<Instant>)>,
    /// 说明气泡与尺寸标签共用一个穿透浮层，消息条单独一个可点击浮层；首次用到时创建。
    tip: Option<tip::Bubble>,
    toast: Option<tip::Bubble>,
    tip_warm_until: Option<Instant>,
    tip_suppressed: Option<&'static str>,
    /// 最近一次渲染时的工具栏宽度、主题和标注工具栏是否展开；轮询定位时使用。
    bar_width: f32,
    light: bool,
    annotation_open: bool,
    scale: f32,
    placement: Placement,
    flyout_above: bool,
    /// 用户拖动后的工具栏左上角（物理像素）；None 时贴靠选区。
    anchor: Option<(i32, i32)>,
    seeking: bool,
    closing: bool,
    hover: Option<Hover>,
    /// 已显示的录制警告数；每条只提示一次。
    warnings_shown: usize,
    /// 刚安排原生定位，短时间内不把位置差异视为用户拖动。
    placing_until: Option<Instant>,
    /// 首帧呈现后再创建选区缩放区。
    resize_handles_pending: bool,
    /// RUSTSHOT_TRACE 时记录打开时刻，首帧渲染打印切换耗时。
    trace_opened: Option<Instant>,
}

fn trace_open(label: &str, since: Instant) {
    if std::env::var_os("RUSTSHOT_TRACE").is_some() {
        eprintln!(
            "RECVIEW {label} {:.1}ms",
            since.elapsed().as_secs_f64() * 1000.
        );
    }
}

impl RecordingView {
    pub fn open(region: ScreenRect, cx: &mut App) -> Result<()> {
        Self::open_with(region, true, cx).map(|_| ())
    }

    /// focus=false 供后台冒烟使用：不激活窗口，不改变用户前台。
    fn open_with(region: ScreenRect, focus: bool, cx: &mut App) -> Result<WindowHandle<Self>> {
        let t0 = Instant::now();
        crate::recording_options::validate_region(region.width, region.height)?;
        let options = RecordingOptions::load();
        let scale = crate::input_overlay::region_scale(region);
        let bar_width = transport_width(None, &options);
        let (placement, flyout_above) =
            toolbar_placement(region, None, bar_width, BAR_HEIGHT, 0., scale);
        let (display_id, bounds) = crate::pin_native::initial_pin_bounds(placement, cx);
        Ok(cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id,
                kind: gpui::WindowKind::PopUp,
                window_background: gpui::WindowBackgroundAppearance::Transparent,
                focus,
                ..Default::default()
            },
            move |window, cx| {
                trace_open("window", t0);
                window.set_window_title(brand_title!("录屏", "Screen recording"));
                let h = hwnd(window);
                let setup = native_setup(h, placement);
                // 只绑定刚创建的自有窗口，先于逻辑聚焦接收组合生命周期。
                let key_owner = KeyOwnerProbe::install(h);
                let border = setup.and_then(|()| {
                    // 缩放区窗口在首帧后再建，工具栏与四条边先出现。
                    let mut border = LongCaptureBorder::new_recording(region)?;
                    border.set_color(IDLE_COLOR)?;
                    border.bind_panel(h)?;
                    border.set_resize_enabled(true)?;
                    Ok(border)
                });
                trace_open("border", t0);
                let focus_handle = cx.focus_handle();
                if focus {
                    window.focus(&focus_handle);
                }
                let focus = focus_handle;
                let (border, status) = match border {
                    Ok(border) => (Some(border), String::new()),
                    Err(error) => (
                        None,
                        trf!(
                            "选区边框不可用：{error}",
                            "Selection border unavailable: {error}"
                        ),
                    ),
                };
                let (key_owner, status) = match key_owner {
                    Ok(owner) => (Some(owner), status),
                    Err(error) => {
                        let message = trf!(
                            "录屏快捷键不可用：{error}",
                            "Recording shortcuts unavailable: {error}"
                        );
                        let status = if status.is_empty() {
                            message
                        } else {
                            trf!("{status}；{message}", "{status}; {message}")
                        };
                        (None, status)
                    }
                };
                let light = crate::theme::is_light(cx);
                let view = cx.new(|cx| Self {
                    region,
                    options,
                    encoder_policy: EncoderPolicy::Auto,
                    quality_profile: None,
                    quality_editor: QualityEditor::new(cx),
                    last_encoding_session: None,
                    device_choices: device_choices::DeviceChoices::default(),
                    audio_input_check: None,
                    audio_input_stopping: false,
                    audio_input_revision: None,
                    record_after_input_check: false,
                    stage: Stage::Idle,
                    recording: None,
                    abandoned: None,
                    result: None,
                    output: None,
                    session_files: output_state::SessionFiles::default(),
                    gif_job: None,
                    levels: MixLevels::default(),
                    mix_job: None,
                    save_owner: volume_intent::SaveOwner::default(),
                    published_copy: None,
                    mix_drag: None,
                    border,
                    border_color: IDLE_COLOR,
                    preview: None,
                    preview_audio_warning: None,
                    drawing: None,
                    countdown: None,
                    flyout: None,
                    region_drag: None,
                    focus,
                    key_owner,
                    picking_save: false,
                    notice: (!status.is_empty()).then(|| Notice {
                        tone: tip::Tone::Error,
                        text: status,
                        until: None,
                    }),
                    flash: None,
                    size_label: None,
                    tip: None,
                    toast: None,
                    tip_warm_until: None,
                    tip_suppressed: None,
                    bar_width,
                    light,
                    annotation_open: false,
                    scale,
                    placement,
                    flyout_above,
                    anchor: None,
                    seeking: false,
                    closing: false,
                    hover: None,
                    warnings_shown: 0,
                    placing_until: None,
                    resize_handles_pending: true,
                    trace_opened: std::env::var_os("RUSTSHOT_TRACE").map(|_| t0),
                });
                let weak = view.downgrade();
                window.on_window_should_close(cx, move |window, cx| {
                    weak.update(cx, |v, cx| v.request_close(window, cx))
                        .unwrap_or(true)
                });
                view.update(cx, |_, cx| {
                    cx.spawn_in(window, async move |view, cx| loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(50))
                            .await;
                        if view.update_in(cx, |v, w, cx| v.poll(w, cx)).is_err() {
                            break;
                        }
                    })
                    .detach();
                });
                view
            },
        )?)
    }

    fn create_resize_handles(&mut self) {
        let start = Instant::now();
        let Some(border) = self.border.as_mut() else {
            return;
        };
        if let Err(error) = border.create_resize_handles() {
            self.fail(trf!(
                "选区缩放不可用：{error}",
                "Region resize unavailable: {error}"
            ));
        }
        trace_open("resize handles", start);
    }

    fn notify(&mut self, tone: tip::Tone, text: impl Into<String>) {
        let seconds = match tone {
            tip::Tone::Info => 4,
            tip::Tone::Warning => 6,
            tip::Tone::Error | tip::Tone::Plain => 0,
        };
        self.notice = Some(Notice {
            tone,
            text: text.into(),
            until: (seconds > 0).then(|| Instant::now() + Duration::from_secs(seconds)),
        });
    }
    /// 进度类消息（正在…、已取消…），约 4 秒。
    fn info(&mut self, text: impl Into<String>) {
        self.notify(tip::Tone::Info, text);
    }
    /// 不影响结果但需要知道的情况，约 6 秒。
    fn warn(&mut self, text: impl Into<String>) {
        self.notify(tip::Tone::Warning, text);
    }
    /// 操作失败：留到点掉或下一次操作。
    fn fail(&mut self, text: impl Into<String>) {
        self.notify(tip::Tone::Error, text);
    }
    /// 成功就地反馈：按钮变对勾，补充说明进按钮提示；先前的错误随之作废。
    fn done(&mut self, id: &'static str, note: impl Into<SharedString>) {
        let now = Instant::now();
        self.flash = Some(Flash {
            id,
            until: now + FLASH,
            note: note.into(),
            note_until: now + FLASH_NOTE,
        });
        if self
            .notice
            .as_ref()
            .is_some_and(|n| n.tone == tip::Tone::Error)
        {
            self.notice = None;
        }
    }
    fn flashing(&self, id: &str) -> bool {
        self.flash
            .as_ref()
            .is_some_and(|f| f.id == id && Instant::now() < f.until)
    }
    fn notice_text(&self) -> String {
        self.notice
            .as_ref()
            .map_or_else(String::new, |n| n.text.clone())
    }

    fn tip_blocked(&self) -> bool {
        self.flyout.is_some()
            || self.region_drag.is_some()
            || self.size_label.is_some()
            || self.picking_save
            || self.mix_drag.is_some()
            || self.seeking
            || self.closing
    }
    fn hover_changed(
        &mut self,
        id: &'static str,
        hint: SharedString,
        hovered: bool,
        window: &Window,
    ) {
        let now = Instant::now();
        if hovered {
            if self.tip_suppressed.is_some_and(|s| s != id) {
                self.tip_suppressed = None;
            }
            if self.tip_suppressed == Some(id) || hint.is_empty() {
                return;
            }
            let text = self
                .flash
                .as_ref()
                .filter(|f| f.id == id && now < f.note_until)
                .map_or(hint, |f| f.note.clone());
            // 进入与离开的先后不固定：从正显示说明的按钮直接移过来也算“紧接着”。
            let warm = self.tip_warm_until.is_some_and(|t| now < t)
                || self.hover.as_ref().is_some_and(|h| h.shown);
            self.hover = Some(Hover {
                id,
                text,
                since: now,
                shown: false,
            });
            if warm {
                self.show_tip(window);
            }
        } else {
            if let Some(hover) = self.hover.take_if(|h| h.id == id) {
                if hover.shown {
                    self.tip_warm_until = Some(now + TIP_WARM);
                }
                self.hide_tip();
            }
            if self.tip_suppressed == Some(id) {
                self.tip_suppressed = None;
            }
        }
    }
    fn tip_pressed(&mut self, id: &'static str) {
        self.hover = None;
        self.hide_tip();
        self.tip_suppressed = Some(id);
        if self
            .notice
            .as_ref()
            .is_some_and(|n| n.tone == tip::Tone::Error)
        {
            self.notice = None;
        }
    }
    fn hide_tip(&mut self) {
        if self.size_label.is_none() {
            if let Some(tip) = self.tip.as_mut() {
                tip.hide();
            }
        }
    }
    fn bubble_look(
        &self,
        text: String,
        tone: tip::Tone,
        mono: bool,
        max_width: i32,
        scale: f32,
    ) -> tip::Look {
        tip::Look {
            text,
            tone,
            mono,
            max_width,
            scale,
            light: self.light,
        }
    }
    /// 说明气泡贴着按钮、在背向选区的一侧（工具栏外侧）；标注工具栏占着外侧时，
    /// 主工具栏按钮的说明改到另一侧。都放不下时换到对侧。
    fn show_tip(&mut self, window: &Window) {
        if self.tip_blocked() {
            return;
        }
        let Some(hover) = self.hover.as_ref() else {
            return;
        };
        let (id, text) = (hover.id, hover.text.to_string());
        let Some(bounds) = button_bounds(id) else {
            return;
        };
        let owner = hwnd(window);
        let Ok(win) = crate::pin_native::rect(owner) else {
            return;
        };
        let scale = window.scale_factor();
        let gap = (6. * scale).round() as i32;
        let center =
            win.x as f32 + (f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.) * scale;
        let bar = (self.bar_height() * scale).round() as i32;
        let bottom = win.y + win.height as i32;
        let (bar_top, bar_bottom) = if self.flyout_above {
            (bottom - bar, bottom)
        } else {
            (win.y, win.y + bar)
        };
        let stacked = self
            .toast
            .as_ref()
            .and_then(tip::Bubble::size)
            .map_or(0, |(_, h)| h as i32 + gap);
        let inward = self.annotation_open && !id.starts_with("ann-");
        let above = self.flyout_above;
        let work = monitor_work((win.x + win.width as i32 / 2, win.y + win.height as i32 / 2));
        let look = self.bubble_look(text, tip::Tone::Plain, false, 0, scale);
        let place = move |(w, h): (u32, u32)| {
            let (w, h) = (w as i32, h as i32);
            let x = (center - w as f32 / 2.).round().clamp(
                work.x as f32,
                (work.x + work.width as i32 - w).max(work.x) as f32,
            ) as i32;
            let fits = |y: i32| y >= work.y && y + h <= work.y + work.height as i32;
            let outward = if above {
                win.y - gap - stacked - h
            } else {
                bottom + gap + stacked
            };
            let opposite = if above { bottom + gap } else { win.y - gap - h };
            let inner = if above {
                bar_bottom + gap
            } else {
                bar_top - gap - h
            };
            let y = if inward && fits(inner) {
                inner
            } else if fits(outward) {
                outward
            } else {
                opposite
            };
            (x, y)
        };
        if self.tip.is_none() {
            match tip::Bubble::new(owner, false) {
                Ok(bubble) => self.tip = Some(bubble),
                Err(error) => {
                    eprintln!("录屏说明浮层不可用：{error}");
                    return;
                }
            }
        }
        if let Some(bubble) = self.tip.as_mut() {
            if let Err(error) = bubble.show(&look, place) {
                eprintln!("录屏说明显示失败：{error}");
                return;
            }
        }
        if let Some(hover) = self.hover.as_mut() {
            hover.shown = true;
        }
    }
    /// 轮询：到时显示说明；指针已离开工具栏（未收到离开事件）时收起。
    fn tip_tick(&mut self, window: &Window) {
        let Some(hover) = self.hover.as_ref() else {
            return;
        };
        let inside = crate::pin_native::rect(hwnd(window)).is_ok_and(|r| {
            let mut cursor = POINT::default();
            unsafe { GetCursorPos(&mut cursor) }.is_ok()
                && cursor.x >= r.x
                && cursor.y >= r.y
                && cursor.x < r.x + r.width as i32
                && cursor.y < r.y + r.height as i32
        });
        if !inside {
            let id = hover.id;
            self.hover_changed(id, SharedString::default(), false, window);
            return;
        }
        if self.tip_blocked() {
            if hover.shown {
                self.hide_tip();
                if let Some(hover) = self.hover.as_mut() {
                    hover.shown = false;
                    hover.since = Instant::now();
                }
            }
            return;
        }
        let delay = if self.stage == Stage::Recording {
            TIP_DELAY_RECORDING
        } else {
            TIP_DELAY
        };
        if !hover.shown && hover.since.elapsed() >= delay {
            self.show_tip(window);
        }
    }
    /// 拖动选区边时在选区左上角外侧显示物理尺寸；上方放不下时放进选区内。
    fn size_tick(&mut self, window: &Window) {
        let Some((region, until)) = self.size_label else {
            return;
        };
        if until.is_some_and(|t| Instant::now() >= t) {
            self.size_label = None;
            if let Some(tip) = self.tip.as_mut() {
                tip.hide();
            }
            return;
        }
        let owner = hwnd(window);
        if self.tip.is_none() {
            self.tip = tip::Bubble::new(owner, false).ok();
        }
        let scale = self.scale;
        let gap = (4. * scale).round() as i32;
        let work = monitor_work((region.left, region.top));
        let look = self.bubble_look(
            size_feedback::size_label(region),
            tip::Tone::Plain,
            true,
            0,
            scale,
        );
        if let Some(bubble) = self.tip.as_mut() {
            let _ = bubble.show(&look, |(w, h)| {
                let above = region.top - gap - h as i32;
                let (x, y) = if above >= work.y {
                    (region.left, above)
                } else {
                    (region.left + gap * 2, region.top + gap * 2)
                };
                let right = work.x.saturating_add(
                    work.width.saturating_sub(w).min(i32::MAX as u32) as i32,
                );
                (x.clamp(work.x, right), y)
            });
        }
    }
    /// 消息条：工具栏外侧左对齐、宽度不超过工具栏，跟随工具栏移动。
    fn notice_tick(&mut self, window: &Window) {
        let now = Instant::now();
        if self.toast.as_mut().is_some_and(tip::Bubble::dismissed) {
            self.notice = None;
        }
        let hovered = self.toast.as_ref().is_some_and(tip::Bubble::hovered);
        if let Some(notice) = self.notice.as_mut() {
            if hovered && notice.tone == tip::Tone::Warning {
                notice.until = Some(
                    notice
                        .until
                        .map_or(now, |t| t.max(now + Duration::from_millis(1500))),
                );
            }
            if notice.until.is_some_and(|t| now >= t) {
                self.notice = None;
            }
        }
        let Some(notice) = self.notice.as_ref() else {
            if let Some(toast) = self.toast.as_mut() {
                toast.hide();
            }
            return;
        };
        let owner = hwnd(window);
        let Ok(win) = crate::pin_native::rect(owner) else {
            return;
        };
        let scale = window.scale_factor();
        let gap = (6. * scale).round() as i32;
        let above = self.flyout_above;
        let work = monitor_work((win.x + win.width as i32 / 2, win.y + win.height as i32 / 2));
        let look = self.bubble_look(
            notice.text.clone(),
            notice.tone,
            false,
            win.width as i32,
            scale,
        );
        if self.toast.is_none() {
            match tip::Bubble::new(owner, true) {
                Ok(bubble) => self.toast = Some(bubble),
                Err(error) => {
                    eprintln!("录屏消息浮层不可用：{error}；{}", notice.text);
                    self.notice = None;
                    return;
                }
            }
        }
        if let Some(toast) = self.toast.as_mut() {
            let _ = toast.show(&look, |(w, h)| {
                let (w, h) = (w as i32, h as i32);
                let x = win
                    .x
                    .clamp(work.x, (work.x + work.width as i32 - w).max(work.x));
                let bottom = win.y + win.height as i32;
                let fits = |y: i32| y >= work.y && y + h <= work.y + work.height as i32;
                let (first, second) = if above {
                    (win.y - gap - h, bottom + gap)
                } else {
                    (bottom + gap, win.y - gap - h)
                };
                (x, if fits(first) { first } else { second })
            });
        }
    }

    fn set_border_color(&mut self, color: u32) {
        if self.border_color != color {
            self.border_color = color;
            if let Some(border) = &self.border {
                let _ = border.set_color(color);
            }
        }
    }

    fn save_options(&mut self) {
        if let Err(error) = self.options.save() {
            self.fail(trf!(
                "无法保存录屏设置：{error}",
                "Cannot save recording settings: {error}"
            ));
        }
    }

    fn phase(&self) -> Option<Phase> {
        self.recording.as_ref().map(|r| r.phase())
    }

    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 工具栏随选区移动或被拖动；按当前位置决定可能被远控时能否不排除采集。
        crate::remote_display::sync_tool_window(hwnd(window), &[self.region]);
        if self.device_choices.poll() {
            cx.notify();
        }
        self.poll_audio_input_check(cx);
        let audio_error = self
            .preview
            .as_ref()
            .and_then(|handle| handle.read(cx).ok())
            .and_then(|view| view.player.audio_error());
        if let Some(error) = audio_error {
            if self.preview_audio_warning.as_ref() != Some(&error) {
                self.preview_audio_warning = Some(error.clone());
                self.warn(trf!(
                    "预览声音不可用：{error}",
                    "Preview audio unavailable: {error}"
                ));
                cx.notify();
            }
        }
        if let Some(handle) = self.drawing {
            let _ = handle.update(cx, |v, w, cx| v.check_input(w, cx));
        }
        if self.flyout == Some(Flyout::Style) && !self.drawing_active(cx) {
            self.flyout = None;
            cx.notify();
        }
        if self
            .flash
            .as_ref()
            .is_some_and(|f| Instant::now() >= f.note_until)
        {
            self.flash = None;
        }
        if let Some((until, _)) = &self.countdown {
            if Instant::now() >= *until {
                self.close_countdown(cx);
                if let Some(recording) = &self.recording {
                    // 设备与编码器已在倒计时期间就绪，归零即采集首帧。
                    recording.go.store(true, Ordering::Release);
                    self.stage = Stage::Recording;
                } else {
                    self.begin_recording(false, cx);
                }
            }
        }
        if self
            .abandoned
            .as_ref()
            .is_some_and(|r| r.outcome.lock().unwrap().is_some())
        {
            self.abandoned = None;
        }
        if let Some(recording) = self.recording.clone() {
            // 设备与编码器就绪、首帧开始采集时才变红，红框即真实录制起点。
            match recording.phase() {
                Phase::Paused => self.set_border_color(PAUSED_COLOR),
                Phase::Starting => {}
                _ => self.set_border_color(RECORDING_COLOR),
            }
            // 完成回执发布前已写入全部警告；先取回执，避免最后一条警告被清掉会话后漏显。
            let outcome = recording.outcome.lock().unwrap().take();
            let warnings = recording.warnings();
            if warnings.len() > self.warnings_shown {
                self.warn(warnings[self.warnings_shown..].join("；"));
                self.warnings_shown = warnings.len();
            }
            if let Some(outcome) = outcome {
                self.recording = None;
                let closing = self.closing;
                self.finish_recording(outcome, window, cx);
                if closing {
                    // 成功和失败收尾都已移除窗口，不能继续访问其几何/焦点。
                    return;
                }
            }
        }
        if let Some(job) = self.gif_job.clone() {
            if let Some(outcome) = job.outcome.lock().unwrap().take() {
                if !output_state::take_current_job(&mut self.gif_job, &job) {
                    drop(outcome);
                    return;
                }
                let current = self.stage == Stage::Preview
                    && !self.closing
                    && job.snapshot.current(
                        self.result.as_ref().map(|result| result.path.as_path()),
                        self.output.as_deref(),
                        self.levels,
                    )
                    && self.save_owner.current(
                        &job.intent,
                        self.result.as_ref().map(|result| result.path.as_path()),
                        self.output.as_deref(),
                        job.request.target(),
                    );
                let outcome = outcome.and_then(|staged| {
                    let same_request = staged.request() == &job.request;
                    staged
                        .commit(|| current && same_request)
                        .map_err(|error| error.to_string())
                });
                self.save_owner.finish(&job.intent);
                match outcome {
                    Ok(path) => {
                        if let Err(error) = self
                            .session_files
                            .gif_published(&job.snapshot.source, &path)
                        {
                            self.warn(trf!(
                                "GIF 已保存，但本次预览不能继续使用它：{error}",
                                "GIF saved, but this preview can't keep using it: {error}"
                            ));
                            return;
                        }
                        self.revoke_pending_output();
                        self.done(
                            "preview-info",
                            trf!(
                                "已转为 GIF · {}",
                                "Converted to GIF · {}",
                                path.file_name().unwrap_or_default().to_string_lossy()
                            ),
                        );
                        self.output = Some(path);
                    }
                    Err(error) => self.fail(error),
                }
            }
        }
        if let Some((job, action)) = self.mix_job.clone() {
            if let Some(outcome) = job.outcome.lock().unwrap().take() {
                if !output_state::take_current_receipt(&mut self.mix_job, &job, &action) {
                    // 迟到回执只释放其自有暂存，不发布或影响后继任务。
                    drop(outcome);
                    return;
                }
                if self.finish_mix(outcome, action, &job, window, cx) {
                    // 真实窗口已移除或已续接关闭，不再访问其几何/焦点。
                    return;
                }
            }
        }
        self.process_region_input();
        self.track_user_move(window);
        self.relayout(window, cx);
        // 说明、消息、尺寸标签都是原生浮层，跟随工具栏当前位置，不触发工具栏重绘。
        self.size_tick(window);
        self.tip_tick(window);
        self.notice_tick(window);
        // 待录且无提示时不需要逐帧刷新，避免空闲重绘。
        if self.stage != Stage::Idle
            || self.flash.is_some()
            || self.region_drag.is_some()
            || self.gif_job.is_some()
            || self.mix_job.is_some()
        {
            cx.notify();
        }
    }

    fn finish_recording(
        &mut self,
        outcome: std::result::Result<RecordingResult, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_drawing(cx);
        self.revoke_pending_output();
        match outcome {
            Ok(result) => {
                self.levels = result.initial_mix;
                if let Err(error) = self
                    .session_files
                    .recording_published_with_mix(&result.path, result.initial_mix)
                {
                    self.warn(trf!(
                        "原录像已保留，但本次修改未能写回：{error}",
                        "The recording is kept, but this change couldn't be written back: {error}"
                    ));
                }
                let auxiliary = result
                    .tracks
                    .iter()
                    .map(|(_, path)| path.as_path())
                    .chain((result.preview != result.path).then_some(result.preview.as_path()));
                if let Err(error) = self.session_files.auxiliary_published(auxiliary) {
                    self.warn(trf!(
                        "部分录屏临时文件未能清理，将保留：{error}",
                        "Some temporary recording files couldn't be cleaned up and will be kept: {error}"
                    ));
                }
                if self.closing {
                    for error in self.session_files.cleanup_auxiliary() {
                        eprintln!("录制关闭后辅助文件清理提示：{error}");
                    }
                    self.session_files.clear();
                    window.remove_window();
                    return;
                }
                self.set_border_color(IDLE_COLOR);
                self.open_result_preview(&result, window, cx);
                self.output = Some(result.path.clone());
                self.result = Some(result);
                self.stage = Stage::Preview;
                self.flyout = None;
            }
            Err(error) => {
                if self.closing {
                    window.remove_window();
                    return;
                }
                // 倒计时期间预备失败：撤下倒计时，回到待录。
                self.close_countdown(cx);
                self.stage = Stage::Idle;
                self.set_border_color(IDLE_COLOR);
                if let Some(border) = self.border.as_mut() {
                    let _ = border.set_resize_enabled(true);
                }
                self.fail(trf!("录屏失败：{error}", "Recording failed: {error}"));
            }
        }
    }

    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.record_after_input_check = false;
        self.stop_audio_input_check();
        match self.stage {
            Stage::Recording => {
                self.closing = true;
                if let Some(recording) = &self.recording {
                    recording.request_stop();
                }
                self.close_drawing(cx);
                self.info(tr!(
                    "正在完成编码，完成后自动关闭…",
                    "Finishing encoding; closes automatically when done…"
                ));
                cx.notify();
                false
            }
            _ => {
                if self.mix_job.is_some() {
                    // 正在写入混音，完成后自动关闭。
                    self.closing = true;
                    return false;
                }
                if let Some(job) = self.gif_job.take() {
                    job.cancel.store(true, Ordering::Release);
                    self.save_owner.finish(&job.intent);
                }
                self.close_countdown(cx);
                self.abandon_prepared();
                self.close_preview(cx);
                if self.mix_needed() && !self.closing {
                    // 先关闭预览释放文件，再把新混音写回录屏历史中的成品。
                    self.closing = true;
                    self.start_mix(MixAction::Close);
                    if self.mix_job.is_some() {
                        self.info(tr!(
                            "正在按调整后的音量写入录屏…",
                            "Writing the recording with the adjusted volume…"
                        ));
                    }
                    cx.notify();
                    return false;
                }
                self.cleanup_session();
                self.border.take();
                let _ = window;
                true
            }
        }
    }

    /// 会话结束：删除 GIF 中间文件和分轨；成品及已发布副本保留。
    fn cleanup_session(&mut self) {
        for error in self.session_files.cleanup_auxiliary() {
            eprintln!("录屏会话辅助文件清理提示：{error}");
        }
        self.published_copy.take();
        self.session_files.clear();
    }

    fn mix_needed(&self) -> bool {
        self.session_files.mix_needed(
            self.result.as_ref().map(|result| result.path.as_path()),
            self.output.as_deref(),
            self.result
                .as_ref()
                .is_some_and(|result| !result.tracks.is_empty()),
            self.levels,
        )
    }

    /// 来源、音量或输出改变时撤销未提交的Copy/Save；已提交成品归历史目录。
    fn revoke_pending_output(&mut self) {
        self.save_owner.invalidate();
        self.published_copy.take();
        if self
            .mix_job
            .as_ref()
            .is_some_and(|(_, action)| matches!(action, MixAction::Save(_) | MixAction::Copy))
        {
            if let Some((job, _)) = self.mix_job.take() {
                job.cancel.store(true, Ordering::Release);
                self.closing = false;
            }
        }
    }

    fn start_mix(&mut self, action: MixAction) {
        if !self.output_gate().worker_slot_available() {
            return;
        }
        let Some(result) = self.result.clone() else {
            return;
        };
        let Some(output) = self.output.clone() else {
            return;
        };
        let levels = self.levels;
        let snapshot = output_state::OutputSnapshot::capture(&result.path, &output, levels);
        let request = match &action {
            MixAction::Save(request) => Some(request.clone()),
            MixAction::Copy => {
                let preset = if output
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("gif"))
                {
                    Preset::Gif
                } else {
                    Preset::CompactMp4
                };
                match recording::new_output_path(preset)
                    .and_then(|target| volume_save::SaveRequest::for_new_output(&target))
                {
                    Ok(request) => Some(request),
                    Err(error) => {
                        self.closing = false;
                        self.fail(trf!(
                            "无法准备复制成品，原文件已保留，可重试：{error}",
                            "Cannot prepare the copy; originals kept. Retry: {error}"
                        ));
                        return;
                    }
                }
            }
            MixAction::Close => match self.session_files.writeback_request(&result.path) {
                Ok(request) => Some(request),
                Err(error) => {
                    self.closing = false;
                    self.fail(trf!(
                        "无法写回音量，现有录屏已保留：{error}",
                        "Cannot write back the volume; the existing recording is kept: {error}"
                    ));
                    return;
                }
            },
        };
        let save_intent = if let Some(request) = &request {
            let captured = self.save_owner.capture(&result.path, &output);
            let Some(intent) = self.save_owner.begin(&captured, request.target()) else {
                return;
            };
            Some(intent)
        } else {
            None
        };
        let needs_mix = self.mix_needed();
        let job = Arc::new(MixJob {
            outcome: Mutex::new(None),
            cancel: AtomicBool::new(false),
            save_intent,
            snapshot,
            request: request.clone(),
        });
        let shared = job.clone();
        let spawned = std::thread::Builder::new()
            .name("rustshot-recording-output".into())
            .spawn(move || {
                let outcome = (|| -> Result<volume_save::StagedSave> {
                    if !needs_mix {
                        let request = request.as_ref().ok_or("输出任务缺少成品目标")?;
                        let staged = volume_save::prepare_copy(request, &output, || {
                            !shared.cancel.load(Ordering::Acquire)
                        })?;
                        return Ok(staged);
                    }
                    let request = request.as_ref().ok_or("混音任务缺少已取得的成品资格")?;
                    volume_save::prepare_with(
                        request,
                        |staged| {
                            crate::preview_audio::remux_recording_tracks(
                                &result.path,
                                staged,
                                &result.tracks,
                                levels,
                                || !shared.cancel.load(Ordering::Acquire),
                            )
                        },
                        || !shared.cancel.load(Ordering::Acquire),
                    )
                })()
                .map_err(|error| error.to_string());
                *shared.outcome.lock().unwrap() = Some(outcome);
            });
        if spawned.is_ok() {
            self.mix_job = Some((job, action));
            self.info(if needs_mix {
                tr!(
                    "正在按调整后的音量混音…",
                    "Mixing with the adjusted volume…"
                )
            } else {
                tr!("正在准备录屏成品…", "Preparing the recording output…")
            });
        } else {
            if let Some(intent) = &job.save_intent {
                self.save_owner.finish(intent);
            }
            self.closing = false;
            self.fail(match action.kind() {
                output_state::OutputKind::Save => tr!(
                    "无法开始另存，原文件已保留，可重试",
                    "Cannot start saving; originals kept. Retry."
                ),
                output_state::OutputKind::Copy => tr!(
                    "无法开始复制，原文件已保留，可重试",
                    "Cannot start copying; originals kept. Retry."
                ),
                output_state::OutputKind::Close => tr!(
                    "无法写回音量，原录屏已保留，可重试",
                    "Cannot write the volume change; the original recording is kept. Retry."
                ),
            });
        }
    }

    fn finish_mix(
        &mut self,
        outcome: std::result::Result<volume_save::StagedSave, String>,
        action: MixAction,
        job: &MixJob,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let kind = action.kind();
        let succeeded = match outcome {
            Ok(staged) => {
                let current = self.stage == Stage::Preview
                    && job.snapshot.current(
                        self.result.as_ref().map(|result| result.path.as_path()),
                        self.output.as_deref(),
                        self.levels,
                    )
                    && job.request.as_ref().is_some_and(|request| {
                        staged.request() == request
                            && job.save_intent.as_ref().is_some_and(|intent| {
                                self.save_owner.current(
                                    intent,
                                    self.result.as_ref().map(|result| result.path.as_path()),
                                    self.output.as_deref(),
                                    request.target(),
                                )
                            })
                    })
                    && (kind != output_state::OutputKind::Close
                        || (self.closing
                            && self.result.as_ref().is_some_and(|result| {
                                self.session_files
                                    .writeback_request(&result.path)
                                    .ok()
                                    .as_ref()
                                    == job.request.as_ref()
                            })));
                let saved = staged.commit(|| current);
                if let Some(intent) = &job.save_intent {
                    self.save_owner.finish(intent);
                }
                match saved {
                    Ok(path) if kind == output_state::OutputKind::Copy => {
                        let copy = output_state::PublishedCopy::from_published(
                            job.snapshot.clone(),
                            &path,
                        );
                        match copy.and_then(|copy| {
                            self.published_copy = Some(copy);
                            copy_file_to_clipboard(hwnd(window), &path)
                        }) {
                            Ok(()) => {
                                self.done(
                                    "preview-copy",
                                    tr!(
                                        "已复制 · 关闭或重录后仍可粘贴",
                                        "Copied · still pastes after closing or recording again"
                                    ),
                                );
                                true
                            }
                            Err(error) => {
                                self.fail(trf!(
                                    "复制失败，成品已保留，可重试：{error}",
                                    "Copy failed; the recording copy is kept. Retry: {error}"
                                ));
                                false
                            }
                        }
                    }
                    Ok(_) if kind == output_state::OutputKind::Close => true,
                    Ok(path) => {
                        let refreshed = job.request.as_ref().map_or(Ok(()), |request| {
                            self.session_files
                                .save_published(request, &path, job.snapshot.levels)
                        });
                        let saved = trf!(
                            "已保存 · {}",
                            "Saved · {}",
                            display_path_text(&path.to_string_lossy())
                        );
                        self.done("preview-save", saved);
                        if let Err(error) = refreshed {
                            self.warn(trf!(
                                "已保存到 {}，但本次预览未能切换到新文件：{error}",
                                "Saved to {}, but this preview couldn't switch to the new file: {error}",
                                path.display()
                            ));
                        }
                        true
                    }
                    Err(error) => {
                        self.fail(if kind == output_state::OutputKind::Close {
                            trf!(
                                "音量调整未能写入，已保留原录屏：{error}",
                                "The volume change couldn't be written; the original recording is kept: {error}"
                            )
                        } else if kind == output_state::OutputKind::Copy {
                            trf!(
                                "复制成品准备失败，原文件已保留，可重试：{error}",
                                "Copy preparation failed; originals kept. Retry: {error}"
                            )
                        } else {
                            trf!(
                                "另存失败，原文件已保留，可重试：{error}",
                                "Save failed; originals kept. Retry: {error}"
                            )
                        });
                        false
                    }
                }
            }
            Err(error) => {
                if let Some(intent) = &job.save_intent {
                    self.save_owner.finish(intent);
                }
                self.fail(match kind {
                    output_state::OutputKind::Save => trf!("另存失败，原文件已保留，可重试：{error}", "Save failed; originals kept. Retry: {error}"),
                    output_state::OutputKind::Copy => trf!("复制成品准备失败，原文件已保留，可重试：{error}", "Copy preparation failed; originals kept. Retry: {error}"),
                    output_state::OutputKind::Close => trf!("音量调整未能写入，已保留原录屏：{error}", "The volume change couldn't be written; the original recording is kept: {error}"),
                });
                false
            }
        };
        match output_state::complete(kind, succeeded, &mut self.closing) {
            output_state::Completion::ResumeClose => {
                self.close(window, cx);
                true
            }
            output_state::Completion::RemoveWindow => {
                self.cleanup_session();
                self.border.take();
                window.remove_window();
                true
            }
            output_state::Completion::CloseFailed => {
                self.cleanup_session();
                self.stage = Stage::Idle;
                self.result = None;
                self.output = None;
                cx.notify();
                false
            }
            output_state::Completion::Preview => {
                cx.notify();
                false
            }
        }
    }

    fn set_level(&mut self, source: AudioSource, x: f32, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        let track_left = self.flyout_left(Flyout::Mix) + 7. + 8.;
        let ratio = ((x - track_left) / SLIDER_WIDTH).clamp(0., 1.);
        let percent = ((ratio * MixLevels::MAX as f32 / 5.).round() * 5.) as u32;
        let mut levels = self.levels;
        match source {
            AudioSource::System => levels.system = percent,
            AudioSource::Microphone => levels.microphone = percent,
        }
        if levels == self.levels {
            return;
        }
        self.revoke_pending_output();
        self.levels = levels;
        self.with_player(cx, |p| p.set_levels(levels));
    }

    /// 下拉左缘对齐所点按钮，放不下时向左收进工具栏。
    fn flyout_left(&self, flyout: Flyout) -> f32 {
        let anchor_id = if flyout == Flyout::EncodingInfo && self.stage == Stage::Idle {
            "rec-preset"
        } else {
            flyout.anchor()
        };
        let anchor = button_bounds(anchor_id).map_or(0., |b| f32::from(b.origin.x));
        anchor.min(self.width() - self.flyout_width(flyout)).max(0.)
    }
    /// 纯选项下拉按最长选项收窄（至少与所点按钮同宽）；带说明文字的面板保持原宽。
    fn flyout_width(&self, flyout: Flyout) -> f32 {
        let labels: Vec<String> = match flyout {
            Flyout::Delay => DELAY_CHOICES
                .iter()
                .map(|delay| delay_label(*delay))
                .chain([tr!("录制延迟", "Start delay").to_string()])
                .collect(),
            Flyout::Fps => FPS_CHOICES.iter().map(|fps| format!("{fps}FPS")).collect(),
            Flyout::Preset if crate::file_launch_command::experimental_media_enabled() => {
                return FLYOUT_WIDTH
            }
            Flyout::Preset => Preset::ALL.iter().map(|p| p.label().to_string()).collect(),
            Flyout::Rate => RATES
                .iter()
                .map(|rate| format!("{}×", trim_rate(*rate)))
                .collect(),
            Flyout::Quality
            | Flyout::Audio
            | Flyout::OutputDevice
            | Flyout::InputDevice
            | Flyout::Input
            | Flyout::Mix => return FLYOUT_WIDTH,
            Flyout::AdvancedQuality | Flyout::EncodingInfo => {
                return 360f32.min(self.width()).max(FLYOUT_WIDTH)
            }
            Flyout::Style => return STYLE_WIDTH,
        };
        let text = labels
            .iter()
            .map(|label| label_width(None, label, 12.))
            .fold(0., f32::max);
        let anchor = button_bounds(flyout.anchor()).map_or(0., |b| f32::from(b.size.width));
        // 行左 6、对勾 14、间距 6、行右 12，面板内边距 4 与边框。
        (text + 6. + 14. + 6. + 12. + 10.).max(anchor).max(96.)
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.request_close(window, cx) {
            window.remove_window();
        }
    }

    fn drawing_active(&self, cx: &App) -> bool {
        self.drawing
            .and_then(|h| h.read(cx).ok().map(|v| v.drawing()))
            .unwrap_or(false)
    }
    fn close_drawing(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.drawing.take() {
            let _ = handle.update(cx, |v, w, _| v.close(w));
        }
        if self.flyout == Some(Flyout::Style) {
            self.flyout = None;
        }
    }
    /// 展开／收起标注工具栏：展开时绘图层接收鼠标，收起后标注仍留在录像里。
    fn toggle_drawing(&mut self, cx: &mut Context<Self>) {
        if self.stage != Stage::Recording
            || self.closing
            || self
                .recording
                .as_ref()
                .is_none_or(|r| !r.phase().active() || r.is_stopping())
        {
            return;
        }
        let result = if let Some(handle) = self.drawing {
            handle
                .update(cx, |v, w, cx| v.set_drawing(!v.drawing(), w, cx))
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })
                .and_then(|r| r)
        } else {
            DrawingGeometry::new(virtual_bounds(), self.region)
                .and_then(|geometry| {
                    drawing_host::DrawingView::open(
                        geometry,
                        self.recording.as_ref().unwrap().clone(),
                        cx,
                    )
                })
                .map(|h| {
                    self.drawing = Some(h);
                })
        };
        if let Err(error) = result {
            self.fail(trf!(
                "无法进入录制标注：{error}",
                "Cannot start markup: {error}"
            ));
        }
        if self.flyout == Some(Flyout::Style) && !self.drawing_active(cx) {
            self.flyout = None;
        }
        cx.notify();
    }
    fn annotations_hidden(&self) -> bool {
        self.recording
            .as_ref()
            .is_some_and(|r| r.annotations.hidden())
    }
    /// 隐藏后屏幕和录像都不画已有标注；开始新的一笔、撤销、重做或清空时自动恢复显示。
    fn toggle_annotations_hidden(&mut self, cx: &mut Context<Self>) {
        let Some(recording) = &self.recording else {
            return;
        };
        recording
            .annotations
            .set_hidden(!recording.annotations.hidden());
        self.with_drawing(cx, |_, cx| cx.notify());
    }
    fn with_drawing(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut drawing_host::DrawingView, &mut Context<drawing_host::DrawingView>),
    ) {
        if let Some(handle) = self.drawing {
            let _ = handle.update(cx, |v, _, cx| f(v, cx));
        }
    }

    fn close_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_audio_warning = None;
        self.seeking = false;
        self.mix_drag = None;
        if matches!(self.flyout, Some(Flyout::Rate | Flyout::Mix)) {
            self.flyout = None;
        }
        if let Some(preview) = self.preview.take() {
            let _ = preview.update(cx, |_, w, _| w.remove_window());
        }
    }

    fn open_result_preview(
        &mut self,
        result: &RecordingResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.close_preview(cx);
        let source = result.path.clone();
        let host = window.window_handle().downcast::<Self>();
        let on_move: crate::recording_preview::MoveHandler =
            std::rc::Rc::new(move |origin, cx| {
                if let Some(host) = host {
                    let _ = host.update(cx, |v, w, cx| {
                        if v.stage == Stage::Preview
                            && v.result.as_ref().is_some_and(|result| {
                                result.path.as_path() == source.as_path()
                            })
                        {
                            v.move_result(origin, w, cx);
                        }
                    });
                }
            });
        let opened = match RecordingPreview::open(
            self.region,
            result.preview.clone(),
            result.tracks.clone(),
            self.levels,
            on_move,
            cx,
        ) {
            Ok(handle) => {
                self.preview_audio_warning = None;
                self.preview = Some(handle);
                true
            }
            Err(error) => {
                self.fail(trf!(
                    "无法打开预览：{error}",
                    "Cannot open the preview: {error}"
                ));
                false
            }
        };
        // 预览窗口创建在后；重新置顶边框和工具栏，保持蓝框与控件可见。
        if let Some(border) = &self.border {
            let _ = border.set_visible(true);
        }
        let _ = crate::pin_native::topmost(hwnd(window), true);
        opened
    }

    fn retry_result_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != Stage::Preview
            || self.closing
            || self.picking_save
            || self.mix_job.is_some()
            || self.gif_job.is_some()
            || self.preview_playback_state(cx) != PreviewPlaybackState::Unavailable
        {
            return;
        }
        let Some(result) = self.result.clone() else {
            return;
        };
        if self.open_result_preview(&result, window, cx) {
            self.info(tr!("正在打开预览…", "Opening preview…"));
        }
    }

    /// 取消倒计时中预备的会话：丢弃输出，线程自行清理文件。
    fn abandon_prepared(&mut self) {
        if self.stage != Stage::Countdown {
            return;
        }
        if let Some(recording) = self.recording.take() {
            recording.discard.store(true, Ordering::Release);
            recording.request_stop();
            self.abandoned = Some(recording);
        }
    }

    fn close_countdown(&mut self, cx: &mut Context<Self>) {
        if let Some((_, handle)) = self.countdown.take() {
            let _ = handle.update(cx, |_, w, _| w.remove_window());
        }
    }

    fn toggle_record(&mut self, cx: &mut Context<Self>) {
        match self.stage {
            Stage::Idle => {
                if self.record_after_input_check {
                    self.record_after_input_check = false;
                    self.info(tr!("已取消开始录制", "Recording start canceled"));
                    return;
                }
                if self
                    .audio_input_check
                    .as_ref()
                    .is_some_and(|check| !check.is_closed())
                {
                    self.stop_audio_input_check();
                    self.record_after_input_check = true;
                    self.flyout = None;
                    self.info(tr!(
                        "正在停止输入检测，随后开始录制；再次按开始可取消",
                        "Stopping input check before recording; press Record again to cancel"
                    ));
                    return;
                }
                self.audio_input_check = None;
                self.audio_input_stopping = false;
                self.audio_input_revision = None;
                self.close_drawing(cx);
                if self.abandoned.is_some() {
                    self.info(tr!(
                        "正在取消上一次录制准备，请稍候",
                        "Canceling the previous start; please wait"
                    ));
                    return;
                }
                self.flyout = None;
                if self.options.delay > 0 {
                    match CountdownView::open(self.region, self.options.delay, cx) {
                        Ok(handle) => {
                            self.countdown = Some((
                                Instant::now() + Duration::from_secs(u64::from(self.options.delay)),
                                handle,
                            ));
                            self.stage = Stage::Countdown;
                            if let Some(border) = self.border.as_mut() {
                                let _ = border.set_resize_enabled(false);
                            }
                            // 倒计时期间完成设备、音频和编码器初始化。
                            self.begin_recording(true, cx);
                            if self.recording.is_none() {
                                self.close_countdown(cx);
                            }
                        }
                        Err(error) => self.warn(trf!(
                            "无法显示倒计时，未开始录制，请重试：{error}",
                            "Cannot show the countdown. Recording did not start; retry: {error}"
                        )),
                    }
                } else {
                    self.begin_recording(false, cx);
                }
            }
            Stage::Countdown => {
                self.close_countdown(cx);
                self.abandon_prepared();
                self.stage = Stage::Idle;
                if let Some(border) = self.border.as_mut() {
                    let _ = border.set_resize_enabled(true);
                }
            }
            Stage::Recording => {
                if let Some(recording) = &self.recording {
                    recording.request_stop();
                }
                self.close_drawing(cx);
            }
            Stage::Preview => {}
        }
    }

    /// prepared 为真时只做初始化，倒计时归零再由 poll 发出开始信号。
    fn begin_recording(&mut self, prepared: bool, cx: &mut Context<Self>) {
        self.revoke_pending_output();
        let experimental = crate::file_launch_command::experimental_media_enabled();
        let encoding = recording::EncodingOptions {
            policy: if experimental {
                self.encoder_policy
            } else {
                EncoderPolicy::Auto
            },
            quality: if experimental && !self.options.preset.is_gif() {
                self.quality_profile
            } else {
                None
            },
        };
        let request = self.quality_editor.request_with_resize(
            self.region,
            &self.options,
            self.quality_profile,
            experimental,
            cx,
        );
        let result = request.and_then(|request| {
            recording::new_output_path(self.options.preset).and_then(|path| {
                if let Some(request) = request {
                    if let Some(resize) = request.resize {
                        if prepared {
                            Recording::prepare_with_resize_request(
                                self.region,
                                self.options.clone(),
                                path,
                                encoding.policy,
                                resize,
                                Some(request.quality),
                                None,
                            )
                        } else {
                            Recording::start_with_resize_request(
                                self.region,
                                self.options.clone(),
                                path,
                                encoding.policy,
                                resize,
                                Some(request.quality),
                                None,
                            )
                        }
                    } else if prepared {
                        Recording::prepare_with_quality_request(
                            self.region,
                            self.options.clone(),
                            path,
                            encoding.policy,
                            request.quality,
                        )
                    } else {
                        Recording::start_with_quality_request(
                            self.region,
                            self.options.clone(),
                            path,
                            encoding.policy,
                            request.quality,
                        )
                    }
                } else if prepared {
                    Recording::prepare_with_encoding_options(
                        self.region,
                        self.options.clone(),
                        path,
                        encoding,
                    )
                } else {
                    Recording::start_with_encoding_options(
                        self.region,
                        self.options.clone(),
                        path,
                        encoding,
                    )
                }
            })
        });
        match result {
            Ok(recording) => {
                self.last_encoding_session = Some(recording.clone());
                self.recording = Some(recording);
                self.warnings_shown = 0;
                if !prepared {
                    self.stage = Stage::Recording;
                }
                self.flyout = None;
                self.region_drag = None;
                if let Some(border) = self.border.as_mut() {
                    border.cancel_input();
                    let _ = border.set_resize_enabled(false);
                }
            }
            Err(error) => {
                self.stage = Stage::Idle;
                if self.quality_editor.error.is_some() {
                    self.flyout = Some(Flyout::AdvancedQuality);
                }
                if let Some(border) = self.border.as_mut() {
                    let _ = border.set_resize_enabled(true);
                }
                self.fail(trf!(
                    "无法开始录屏：{error}",
                    "Cannot start recording: {error}"
                ));
            }
        }
    }

    fn toggle_pause(&mut self) {
        if self.stage != Stage::Recording {
            return;
        }
        if let Some(recording) = &self.recording {
            if recording.phase().active() {
                recording.toggle_pause();
            }
        }
    }

    fn rerecord(&mut self, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        self.revoke_pending_output();
        if let Some(job) = self.gif_job.take() {
            job.cancel.store(true, Ordering::Release);
        }
        self.close_preview(cx);
        if self.result.take().is_some() {
            for error in self.session_files.cleanup_auxiliary() {
                eprintln!("录屏重录辅助文件清理提示：{error}");
            }
            if let Err(error) = self.session_files.remove_gif_for_rerecord() {
                self.warn(trf!(
                    "本会话 GIF 未能清理，文件已保留：{error}",
                    "The session GIF could not be cleaned up; the file is kept: {error}"
                ));
            }
            if let Err(error) = self.session_files.remove_original_for_rerecord() {
                self.warn(trf!(
                    "本会话原录像未能清理，文件已保留：{error}",
                    "The session recording could not be cleaned up; the file is kept: {error}"
                ));
            }
            self.output.take();
        }
        self.session_files.clear();
        self.stage = Stage::Idle;
        self.flyout = None;
        self.set_border_color(IDLE_COLOR);
        if let Some(border) = self.border.as_mut() {
            let _ = border.set_visible(true);
            let _ = border.set_resize_enabled(true);
        }
    }

    fn convert_gif(&mut self) {
        if self.closing {
            return;
        }
        self.revoke_pending_output();
        if let Some(job) = self.gif_job.take() {
            job.cancel.store(true, Ordering::Release);
            self.info(tr!("已取消 GIF 转换", "GIF conversion canceled"));
            return;
        }
        let Some(result) = &self.result else {
            return;
        };
        if let Some(target) = self.session_files.reusable_gif(&result.path) {
            self.output = Some(target.into());
            self.info(tr!(
                "已切换到本会话的 GIF",
                "Switched to this session's GIF"
            ));
            return;
        }
        if !self.output_gate().worker_slot_available() {
            return;
        }
        let request =
            match output_state::new_gif_request(|| recording::new_output_path(Preset::Gif)) {
                Ok(request) => request,
                Err(error) => {
                    self.fail(trf!(
                        "无法准备 GIF 新成品，现有文件已保留：{error}",
                        "Cannot prepare a new GIF; existing files are kept: {error}"
                    ));
                    return;
                }
            };
        let Some(output) = &self.output else {
            return;
        };
        let snapshot = output_state::OutputSnapshot::capture(&result.path, output, self.levels);
        let captured = self.save_owner.capture(&result.path, output);
        let Some(intent) = self.save_owner.begin(&captured, request.target()) else {
            return;
        };
        let job = Arc::new(GifJob {
            progress: AtomicU32::new(0),
            cancel: AtomicBool::new(false),
            outcome: Mutex::new(None),
            snapshot,
            request: request.clone(),
            intent,
        });
        let shared = job.clone();
        let source = result.preview.clone();
        let fps = self.options.fps;
        let spawned = std::thread::Builder::new()
            .name("rustshot-gif-export".into())
            .spawn(move || {
                let outcome = crate::media_foundation::MfRuntime::new()
                    .and_then(|_runtime| {
                        volume_save::prepare_with(
                            &request,
                            |staged| {
                                crate::gif_export::mp4_to_gif(&source, staged, fps, |progress| {
                                    shared.progress.store(progress.to_bits(), Ordering::Relaxed);
                                    !shared.cancel.load(Ordering::Acquire)
                                })
                            },
                            || !shared.cancel.load(Ordering::Acquire),
                        )
                    })
                    .map_err(|e| e.to_string());
                *shared.outcome.lock().unwrap() = Some(outcome);
            });
        if spawned.is_ok() {
            self.gif_job = Some(job);
        } else {
            self.save_owner.finish(&job.intent);
            self.fail(tr!(
                "无法开始 GIF 转换，现有文件已保留，可重试",
                "Cannot start GIF conversion; existing files are kept. Retry."
            ));
        }
    }

    fn output_gate(&self) -> output_state::OutputGate {
        output_state::OutputGate {
            preview: self.stage == Stage::Preview,
            has_output: self.output.is_some(),
            gif_busy: self.gif_job.is_some(),
            output_busy: self.mix_job.is_some(),
            closing: self.closing,
            picking_save: self.picking_save,
            gesture_busy: self.seeking || self.mix_drag.is_some() || self.region_drag.is_some(),
        }
    }

    fn preview_output_ready(&self) -> bool {
        self.output_gate().ready()
    }

    fn save_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.preview_output_ready() {
            return;
        }
        let Some(source) = self.output.clone() else {
            return;
        };
        let Some(recording_source) = self.result.as_ref().map(|result| result.path.clone()) else {
            return;
        };
        let snapshot = self.save_owner.capture(&recording_source, &source);
        let name = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| tr!("录屏.mp4", "Recording.mp4").into());
        let directory = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Videos");
        self.picking_save = true;
        cx.notify();
        let receiver = cx.prompt_for_new_path(&directory, Some(&name));
        cx.spawn_in(window, async move |view, cx| {
            let chosen = receiver.await;
            let _ = view.update(cx, |v, cx| {
                // 取消、打开失败、receiver 中断均恢复；不让对话框资格残留。
                v.picking_save = false;
                cx.notify();
                if !v.output_gate().dialog_current(
                    &v.save_owner,
                    &snapshot,
                    v.result.as_ref().map(|result| result.path.as_path()),
                    v.output.as_deref(),
                ) {
                    return;
                }
                let message = match chosen {
                    Ok(Ok(Some(target))) => {
                        let chosen_target = target.clone();
                        let target = match source.extension() {
                            Some(ext) if target.extension().is_none() => target.with_extension(ext),
                            _ => target,
                        };
                        match volume_save::SaveRequest::from_dialog(&chosen_target, &target) {
                            Ok(request) => {
                                v.start_mix(MixAction::Save(request));
                                cx.notify();
                                return;
                            }
                            Err(error) => trf!("无法另存：{error}", "Cannot save: {error}"),
                        }
                    }
                    Ok(Ok(None)) | Err(_) => return,
                    Ok(Err(error)) => trf!(
                        "无法打开保存对话框：{error}",
                        "Cannot open the save dialog: {error}"
                    ),
                };
                v.fail(message);
                cx.notify();
            });
        })
        .detach();
    }

    fn copy_file(&mut self, window: &Window) {
        if !self.preview_output_ready() {
            return;
        }
        let source = self.result.as_ref().map(|result| result.path.as_path());
        let path = self
            .published_copy
            .as_ref()
            .filter(|published| published.current(source, self.output.as_deref(), self.levels))
            .map(|published| published.path.clone());
        let Some(path) = path else {
            self.published_copy.take();
            self.start_mix(MixAction::Copy);
            return;
        };
        match copy_file_to_clipboard(hwnd(window), &path) {
            Ok(()) => self.done(
                "preview-copy",
                tr!(
                    "已复制 · 关闭或重录后仍可粘贴",
                    "Copied · still pastes after closing or recording again"
                ),
            ),
            Err(error) => self.fail(trf!(
                "复制失败，成品已保留，可重试：{error}",
                "Copy failed; the recording copy is kept. Retry: {error}"
            )),
        }
    }

    fn open_history(&mut self, cx: &mut Context<Self>) {
        let _ = cx;
        if let Err(error) = crate::settings_view::main_page::launch(
            crate::settings_view::main_page::MainPage::History,
        ) {
            self.fail(trf!(
                "无法打开录屏历史：{error}",
                "Cannot open recording history: {error}"
            ));
        }
    }

    fn process_region_input(&mut self) {
        let Some(border) = self.border.as_ref() else {
            return;
        };
        let events = border.drain_events();
        for event in events {
            match event {
                RegionInputEvent::Down { id, handle, point } => {
                    if self.stage != Stage::Idle || handle == RegionHandle::Move {
                        if let Some(border) = &self.border {
                            border.cancel_input();
                        }
                        continue;
                    }
                    match RegionGesture::new_for_axis(
                        id,
                        self.region,
                        point,
                        handle,
                        None,
                        virtual_bounds(),
                        ScrollAxis::Vertical,
                        0,
                    ) {
                        Ok(gesture) => self.region_drag = Some((gesture, self.region)),
                        Err(error) => {
                            if let Some(border) = &self.border {
                                border.cancel_input();
                            }
                            self.warn(error.to_string());
                        }
                    }
                }
                RegionInputEvent::Move { id, point } => {
                    let Some((gesture, _)) =
                        self.region_drag.as_ref().filter(|(g, _)| g.id() == id)
                    else {
                        continue;
                    };
                    if let Ok(region) = gesture.update(point) {
                        if let Some(border) = self.border.as_mut() {
                            let _ = border.update_region(region);
                        }
                        self.hover = None;
                        self.size_label = Some((region, None));
                    }
                }
                RegionInputEvent::Up { id, point } => {
                    let Some((gesture, original)) = self.region_drag.take_if(|(g, _)| g.id() == id)
                    else {
                        continue;
                    };
                    self.release_size_label();
                    let region = gesture.update(point).unwrap_or(original);
                    match crate::recording_options::validate_region(region.width, region.height) {
                        Ok(()) => {
                            self.region = region;
                            self.anchor = None;
                        }
                        Err(error) => {
                            if let Some(border) = self.border.as_mut() {
                                let _ = border.update_region(original);
                            }
                            self.warn(error.to_string());
                        }
                    }
                }
                RegionInputEvent::Cancel { id } => {
                    if let Some((_, original)) = self.region_drag.take_if(|(g, _)| g.id() == id) {
                        self.release_size_label();
                        if let Some(border) = self.border.as_mut() {
                            let _ = border.update_region(original);
                        }
                    }
                }
            }
        }
    }
    /// 松手后尺寸标签再停 0.4 秒。
    fn release_size_label(&mut self) {
        if let Some((_, until)) = self.size_label.as_mut() {
            *until = Some(Instant::now() + Duration::from_millis(400));
        }
    }

    fn bar_height(&self) -> f32 {
        if self.stage == Stage::Preview {
            BAR_HEIGHT + SEEK_HEIGHT
        } else {
            BAR_HEIGHT
        }
    }

    /// 工具栏窗口在主栏之外的高度：标注工具栏与下拉面板。说明和消息都在独立浮层里，不计入。
    fn extra_height(&self) -> f32 {
        let annotation = if self.annotation_open {
            ANNOTATION_HEIGHT + 4.
        } else {
            0.
        };
        annotation + self.flyout.map_or(0., |f| f.height() + 4.)
    }

    /// 最近一次渲染按按钮内容算出的宽度。
    fn width(&self) -> f32 {
        self.bar_width
    }

    /// 拖动预览画面后整体平移：选区、边框与工具栏（含用户拖过的锚点）一起跟随。
    /// 重录沿用移动后的位置。
    fn move_result(&mut self, origin: (i32, i32), window: &mut Window, cx: &mut Context<Self>) {
        if self.stage != Stage::Preview {
            return;
        }
        let (dx, dy) = (origin.0 - self.region.left, origin.1 - self.region.top);
        if (dx, dy) == (0, 0) {
            return;
        }
        self.region.left = origin.0;
        self.region.top = origin.1;
        if let Some(border) = self.border.as_mut() {
            let _ = border.update_region(self.region);
        }
        if let Some((x, y)) = self.anchor {
            self.anchor = Some((x + dx, y + dy));
        }
        self.scale = crate::input_overlay::region_scale(self.region);
        self.relayout(window, cx);
        cx.notify();
    }

    fn relayout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (placement, above) = toolbar_placement(
            self.region,
            self.anchor,
            self.width(),
            self.bar_height(),
            self.extra_height(),
            self.scale,
        );
        self.flyout_above = above;
        if placement != self.placement {
            self.placement = placement;
            self.placing_until = Some(Instant::now() + Duration::from_millis(400));
            crate::pin_native::place_later(hwnd(window), placement, Some(true), false, cx);
        }
    }

    /// 用户通过把手拖动整条工具栏后，记录新的工具栏锚点。
    fn track_user_move(&mut self, window: &Window) {
        if self.placing_until.is_some_and(|t| Instant::now() < t) {
            return;
        }
        let Ok(rect) = crate::pin_native::rect(hwnd(window)) else {
            return;
        };
        if (rect.x, rect.y) == (self.placement.x, self.placement.y)
            || rect.width != self.placement.width
            || rect.height != self.placement.height
        {
            return;
        }
        let extra = (self.extra_height() * self.scale).round() as i32;
        let bar_y = if self.flyout_above {
            rect.y + extra
        } else {
            rect.y
        };
        self.anchor = Some((rect.x, bar_y));
        self.placement.x = rect.x;
        self.placement.y = rect.y;
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        let drawing = self.stage == Stage::Recording && self.drawing_active(cx);
        if drawing
            && !modifiers.function
            && !event.is_held
            && self.focus.is_focused(window)
            && window.is_window_active()
            && self
                .key_owner
                .as_ref()
                .is_some_and(|p| p.owns_keyboard() && !p.composing())
        {
            if let Some(action) = recording::live_annotation::edit_key(
                &event.keystroke.key,
                modifiers.control,
                modifiers.shift,
                modifiers.alt,
                modifiers.platform,
            ) {
                if let Some(handle) = self.drawing {
                    let _ = handle.update(cx, |v, _, cx| match action {
                        recording::live_annotation::EditKey::Undo => v.undo(cx),
                        recording::live_annotation::EditKey::Redo => v.redo(cx),
                        recording::live_annotation::EditKey::Delete => v.delete_selected(cx),
                        recording::live_annotation::EditKey::Clear => v.clear(cx),
                    });
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
            }
        }
        let context = ShortcutContext {
            stage: match self.stage {
                Stage::Idle => ShortcutStage::Idle,
                Stage::Countdown => ShortcutStage::Countdown,
                Stage::Recording => ShortcutStage::Recording,
                Stage::Preview => ShortcutStage::Preview,
            },
            // 精确根焦点；子文本控件、其它窗口或原生对话框拥有输入时让行。
            keyboard_owned: self.focus.is_focused(window)
                && window.is_window_active()
                && self
                    .key_owner
                    .as_ref()
                    .is_some_and(KeyOwnerProbe::owns_keyboard),
            ime_composing: self.key_owner.as_ref().is_none_or(KeyOwnerProbe::composing),
            native_dialog: self.picking_save,
            flyout_open: self.flyout.is_some(),
            drawing,
            output_ready: self.preview_output_ready(),
        };
        let action = match shortcut_keys::route(
            &event.keystroke.key,
            ShortcutModifiers {
                control: modifiers.control,
                alt: modifiers.alt,
                shift: modifiers.shift,
                platform: modifiers.platform,
                function: modifiers.function,
            },
            event.is_held,
            context,
        ) {
            ShortcutRoute::Pass => return,
            ShortcutRoute::Consume => {
                cx.stop_propagation();
                return;
            }
            ShortcutRoute::Run(action) => action,
        };
        cx.stop_propagation();
        match action {
            ShortcutAction::ToggleRecord => self.toggle_record(cx),
            ShortcutAction::TogglePause => self.toggle_pause(),
            ShortcutAction::TogglePlayback => {
                if self.preview_playback_ready(cx) {
                    self.with_player(cx, |p| p.toggle());
                }
            }
            ShortcutAction::DismissFlyout => {
                self.flyout = None;
                self.mix_drag = None;
            }
            ShortcutAction::ExitDrawing => self.toggle_drawing(cx),
            ShortcutAction::CopyFile => self.copy_file(window),
            ShortcutAction::SaveAs => self.save_as(window, cx),
            ShortcutAction::Done | ShortcutAction::Close => {
                self.close(window, cx);
                return;
            }
        }
        cx.notify();
    }

    fn with_player<R>(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut crate::recording_preview::Player) -> R,
    ) -> Option<R> {
        let preview = self.preview?;
        preview
            .update(cx, |v, _, cx| {
                let result = f(&mut v.player);
                cx.notify();
                result
            })
            .ok()
    }

    fn player_state(&self, cx: &App) -> (bool, Duration, Duration, f32, (u32, u32)) {
        self.preview
            .and_then(|p| p.read(cx).ok())
            .map(|v| {
                let (position, duration) = v.player.position();
                (
                    v.player.playing(),
                    position,
                    duration,
                    v.player.rate,
                    v.player.size(),
                )
            })
            .unwrap_or((false, Duration::ZERO, Duration::ZERO, 1., (0, 0)))
    }

    fn preview_playback_state(&self, cx: &App) -> PreviewPlaybackState {
        let Some(preview) = self.preview.and_then(|handle| handle.read(cx).ok()) else {
            return PreviewPlaybackState::Unavailable;
        };
        if preview.player.error().is_some() {
            return PreviewPlaybackState::Unavailable;
        }
        let (width, height) = preview.player.size();
        if width == 0 || height == 0 {
            PreviewPlaybackState::Opening
        } else {
            PreviewPlaybackState::Ready
        }
    }

    fn preview_playback_ready(&self, cx: &App) -> bool {
        self.stage == Stage::Preview
            && !self.closing
            && self.preview_playback_state(cx) == PreviewPlaybackState::Ready
    }

    fn toggle_flyout(&mut self, flyout: Flyout) {
        self.flyout = if self.flyout == Some(flyout) {
            None
        } else {
            Some(flyout)
        };
        if crate::file_launch_command::experimental_media_enabled()
            && !self.options.preset.is_gif()
            && matches!(
                self.flyout,
                Some(Flyout::Audio | Flyout::OutputDevice | Flyout::InputDevice)
            )
        {
            self.device_choices.refresh();
        }
    }

    fn audio_selection(&self, kind: EndpointKind) -> &EndpointSelection {
        match kind {
            EndpointKind::Render => &self.options.system_output,
            EndpointKind::Capture => &self.options.microphone_input,
        }
    }

    fn choose_audio_device(
        &mut self,
        kind: EndpointKind,
        selection: EndpointSelection,
        cx: &mut Context<Self>,
    ) {
        if self.stage != Stage::Idle || self.options.preset.is_gif() {
            return;
        }
        if self.audio_selection(kind) != &selection {
            self.stop_audio_input_check();
            match kind {
                EndpointKind::Render => self.options.system_output = selection,
                EndpointKind::Capture => self.options.microphone_input = selection,
            }
            self.save_options();
        }
        self.flyout = Some(Flyout::Audio);
        cx.notify();
    }

    fn audio_device_selector(
        &self,
        kind: EndpointKind,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (id, label, flyout) = match kind {
            EndpointKind::Render => (
                "rec-output-device",
                tr!("系统输出", "Output"),
                Flyout::OutputDevice,
            ),
            EndpointKind::Capture => (
                "rec-input-device",
                tr!("麦克风输入", "Microphone"),
                Flyout::InputDevice,
            ),
        };
        let selected = self
            .device_choices
            .selection_label(kind, self.audio_selection(kind));
        let enabled = !self.options.preset.is_gif();
        with_tip(
            div()
                .id(id)
                .h(px(28.))
                .px(px(6.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded_md()
                .text_size(px(11.))
                .text_color(rgb(if enabled { p.foreground } else { p.muted }))
                .when(enabled, |row| {
                    row.cursor_pointer().hover(move |row| row.bg(rgb(p.hover)))
                })
                .child(div().w(px(72.)).flex_shrink_0().child(label))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(selected.clone()),
                )
                .child("▸")
                .on_click(cx.listener(move |view, _, _, cx| {
                    if enabled && view.stage == Stage::Idle {
                        view.toggle_flyout(flyout);
                        cx.notify();
                    }
                })),
            id,
            selected.into(),
            enabled,
        )
    }

    fn audio_device_panel(
        &self,
        mut panel: gpui::Div,
        kind: EndpointKind,
        p: Palette,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let title = match kind {
            EndpointKind::Render => tr!("系统输出设备", "System output"),
            EndpointKind::Capture => tr!("麦克风设备", "Microphone"),
        };
        panel = panel.child(
            div()
                .h(px(22.))
                .flex_shrink_0()
                .px(px(6.))
                .flex()
                .items_center()
                .justify_between()
                .text_size(px(11.))
                .text_color(rgb(p.muted))
                .child(title)
                .child(
                    div()
                        .id("rec-refresh-devices")
                        .cursor_pointer()
                        .child(tr!("刷新", "Refresh"))
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.device_choices.refresh();
                            cx.notify();
                        })),
                ),
        );
        let mut rows = div()
            .id("rec-audio-device-rows")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll();
        rows = rows.child(
            option_row(
                "rec-device-default".into(),
                tr!("跟随系统默认", "System default"),
                self.audio_selection(kind) == &EndpointSelection::Default,
                p,
            )
            .on_click(cx.listener(move |view, _, _, cx| {
                view.choose_audio_device(kind, EndpointSelection::Default, cx)
            })),
        );
        let entries = self.device_choices.entries(kind);
        for (index, entry) in entries.iter().enumerate() {
            let selection = EndpointSelection::Fixed {
                id: entry.id.clone(),
            };
            let selected = self.audio_selection(kind) == &selection;
            let label = device_choices::option_label(entry, entries);
            rows = rows.child(
                option_row(format!("rec-device-{index}").into(), "", selected, p)
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(label),
                    )
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.choose_audio_device(kind, selection.clone(), cx)
                    })),
            );
        }
        if self
            .device_choices
            .saved_missing(kind, self.audio_selection(kind))
        {
            rows = rows.child(
                div()
                    .h(px(30.))
                    .px(px(6.))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(tr!(
                        "所选设备暂不可用，选择已保留",
                        "Saved device unavailable; choice kept"
                    )),
            );
        }
        if self.device_choices.loading() {
            rows = rows.child(
                div()
                    .px(px(6.))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(tr!("正在读取设备…", "Loading devices…")),
            );
        } else if let Some(error) = self.device_choices.error(kind) {
            rows = rows.child(
                div()
                    .px(px(6.))
                    .text_size(px(11.))
                    .text_color(rgb(RED))
                    .child(error.to_string()),
            );
        } else if entries.is_empty() {
            rows = rows.child(
                div()
                    .px(px(6.))
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(tr!("没有可用设备", "No available devices")),
            );
        }
        panel
            .child(rows)
            .child(div().h(px(38.)).flex_shrink_0().child(hint_text(
                tr!(
                    "不可用的固定设备会临时回退默认，已保存的选择不变。",
                    "Unavailable fixed devices fall back to default; the saved choice is kept."
                ),
                p,
            )))
    }
}

/// 有说明的控件共用：记录位置、悬停出说明（不可用时不出）、按下收起说明。
fn with_tip(
    element: gpui::Stateful<gpui::Div>,
    id: &'static str,
    hint: SharedString,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    element
        .relative()
        .child(bounds_probe(id))
        .on_hover(move |hovered, window, cx| {
            set_hover(window, cx, id, hint.clone(), *hovered && enabled)
        })
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            tip_pressed(window, cx, id)
        })
}

fn button_shell(
    id: &'static str,
    hint: impl Into<SharedString>,
    enabled: bool,
    p: Palette,
) -> gpui::Stateful<gpui::Div> {
    let element = div()
        .id(id)
        .size(px(30.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_md()
        .opacity(if enabled { 1. } else { 0.35 })
        .when(enabled, |d| {
            d.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
        });
    with_tip(element, id, hint.into(), enabled)
}

fn icon_button(
    id: &'static str,
    icon: Icon,
    color: u32,
    hint: impl Into<SharedString>,
    enabled: bool,
    p: Palette,
) -> gpui::Stateful<gpui::Div> {
    button_shell(id, hint, enabled, p).child(toolbar_icons::action(icon, color).size(px(18.)))
}

/// 开关型图标按钮：开启时选中底色加强调色。
fn toggle_button(
    id: &'static str,
    icon: Icon,
    hint: impl Into<SharedString>,
    enabled: bool,
    active: bool,
    p: Palette,
) -> gpui::Stateful<gpui::Div> {
    icon_button(
        id,
        icon,
        if active { p.accent } else { p.foreground },
        hint,
        enabled,
        p,
    )
    .when(active, |d| d.bg(rgb(p.selected)))
}

fn chip(
    id: &'static str,
    label: impl IntoElement,
    hint: impl Into<SharedString>,
    enabled: bool,
    active: bool,
    p: Palette,
) -> gpui::Stateful<gpui::Div> {
    let element = div()
        .id(id)
        .h(px(28.))
        .px(px(6.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(2.))
        .rounded_md()
        .text_size(px(12.))
        .text_color(rgb(if active { p.accent } else { p.foreground }))
        .bg(if active { rgb(p.selected) } else { rgba(0) })
        .opacity(if enabled { 1. } else { 0.4 })
        .when(enabled, |d| {
            d.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
        })
        .child(label);
    with_tip(element, id, hint.into(), enabled)
}

/// 下拉选项：选中项强调色加对勾，不铺底色。
fn option_row(
    id: SharedString,
    label: impl Into<SharedString>,
    selected: bool,
    p: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(28.))
        .pl(px(6.))
        .pr(px(12.))
        .flex()
        .items_center()
        .gap(px(6.))
        .rounded_md()
        .cursor_pointer()
        .text_size(px(12.))
        .whitespace_nowrap()
        .text_color(rgb(if selected { p.accent } else { p.foreground }))
        .hover(move |s| s.bg(rgb(p.hover)))
        .child(
            div()
                .size(px(14.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .when(selected, |d| {
                    d.child(toolbar_icons::action(Icon::Check, p.accent).size(px(14.)))
                }),
        )
        .child(label.into())
}

fn check_row(
    id: &'static str,
    label: &'static str,
    checked: bool,
    enabled: bool,
    p: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(28.))
        .px(px(8.))
        .flex()
        .items_center()
        .gap(px(8.))
        .rounded_md()
        .text_size(px(12.))
        .text_color(rgb(p.foreground))
        .opacity(if enabled { 1. } else { 0.4 })
        .when(enabled, |d| {
            d.cursor_pointer().hover(move |s| s.bg(rgb(p.hover)))
        })
        .child(
            div()
                .size(px(14.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .border_1()
                .border_color(rgb(if checked { p.accent } else { p.muted }))
                .bg(if checked { rgb(p.accent) } else { rgba(0) })
                .when(checked, |d| {
                    d.child(toolbar_icons::action(Icon::Confirm, p.background).size(px(12.)))
                }),
        )
        .child(label)
}

fn hint_text(text: &'static str, p: Palette) -> gpui::Div {
    div()
        .px(px(8.))
        .py(px(2.))
        .text_size(px(11.))
        .text_color(rgb(p.muted))
        .child(text)
}

impl RecordingView {
    /// 待录／录制工具栏：设置组（计时、帧率、格式、声音、按键显示）｜录制组（录制、暂停、标注、显示隐藏）｜关闭。
    /// 宽度按内容计算（transport_width），录制前后不变；弹性空白只吸收估算余量。
    fn transport_bar(&self, p: Palette, cx: &mut Context<Self>) -> gpui::Div {
        let idle = self.stage == Stage::Idle;
        let recording = self.stage == Stage::Recording;
        let phase = self.phase();
        let paused = phase == Some(Phase::Paused);
        let finishing = matches!(
            phase,
            Some(Phase::Finishing | Phase::Converting | Phase::Done)
        );
        // 收尾进度放在计时器的位置，不额外占宽。
        let timer = if finishing {
            match phase {
                Some(Phase::Converting) => format!(
                    "GIF {:.0}%",
                    self.recording.as_ref().map_or(0., |r| r.progress() * 100.)
                ),
                _ => tr!("编码中…", "Encoding…").into(),
            }
        } else {
            match self.stage {
                Stage::Countdown => self.countdown.as_ref().map_or_else(
                    || "0:00".into(),
                    |(until, _)| {
                        format!(
                            "-{}s",
                            until.saturating_duration_since(Instant::now()).as_secs() + 1
                        )
                    },
                ),
                Stage::Recording => self
                    .recording
                    .as_ref()
                    .map_or_else(|| "0:00".into(), |r| format_clock(r.elapsed())),
                _ => "0:00".into(),
            }
        };
        let delay_hint = if self.options.delay == 0 {
            tr!("延迟开始 · 关", "Start delay · Off").to_string()
        } else {
            trf!("延迟开始 · {} 秒", "Start delay · {}s", self.options.delay)
        };
        let audio_icon = if self.options.audio_active() {
            Icon::Volume
        } else {
            Icon::VolumeMute
        };
        let mut bar = div()
            .h(px(BAR_HEIGHT))
            .px(px(6.))
            .flex()
            .items_center()
            .gap(px(2.));
        bar = bar.child(grip(p));
        bar = bar.child(
            chip(
                "rec-timer",
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .child(
                        div()
                            .font_family("Consolas")
                            .min_w(px(TIMER_WIDTH))
                            .whitespace_nowrap()
                            .child(timer),
                    )
                    // 下拉箭头始终占位，开始录制前后宽度一致。
                    .child(
                        toolbar_icons::action(
                            Icon::Chevron,
                            if idle { p.muted } else { p.background },
                        )
                        .size(px(10.))
                        .when(!idle, |icon| icon.opacity(0.)),
                    ),
                delay_hint,
                idle,
                self.flyout == Some(Flyout::Delay),
                p,
            )
            .on_click(cx.listener(move |v, _, _, cx| {
                if v.stage == Stage::Idle {
                    v.toggle_flyout(Flyout::Delay);
                    cx.notify();
                }
            })),
        );
        bar = bar.child(
            chip(
                "rec-fps",
                format!("{}FPS", self.options.fps),
                tr!("帧率", "Frame rate"),
                idle,
                self.flyout == Some(Flyout::Fps),
                p,
            )
            .on_click(cx.listener(move |v, _, _, cx| {
                if v.stage == Stage::Idle {
                    v.toggle_flyout(Flyout::Fps);
                    cx.notify();
                }
            })),
        );
        let mut format_hint = if crate::file_launch_command::experimental_media_enabled()
            && !self.options.preset.is_gif()
        {
            self.quality_profile.map_or_else(
                || tr!("录制格式", "Format").to_string(),
                |profile| {
                    trf!("录制格式；画质：{}", "Format; quality: {}", profile.label())
                },
            )
        } else {
            tr!("录制格式", "Format").to_string()
        };
        if let Some(hint) = size_feedback::crop_hint(self.region) {
            format_hint.push('\n');
            format_hint.push_str(&hint);
        }
        bar = bar.child(
            chip(
                "rec-preset",
                self.options.preset.label(),
                format_hint,
                idle,
                matches!(self.flyout, Some(Flyout::Preset | Flyout::Quality)),
                p,
            )
            .on_click(cx.listener(move |v, _, _, cx| {
                if v.stage == Stage::Idle {
                    v.toggle_flyout(Flyout::Preset);
                    cx.notify();
                }
            })),
        );
        bar = bar.child(
            icon_button(
                "rec-audio",
                audio_icon,
                if self.options.audio_active() {
                    p.accent
                } else {
                    p.foreground
                },
                tr!("声音", "Sound"),
                idle,
                p,
            )
            .on_click(cx.listener(|v, _, _, cx| {
                if v.stage == Stage::Idle {
                    v.toggle_flyout(Flyout::Audio);
                    cx.notify();
                }
            })),
        );
        bar = bar.child(
            icon_button(
                "rec-input",
                Icon::Keyboard,
                if self.options.input_overlay_active() {
                    p.accent
                } else {
                    p.foreground
                },
                tr!("显示按键和鼠标", "Show keys and mouse"),
                idle,
                p,
            )
            .on_click(cx.listener(|v, _, _, cx| {
                if v.stage == Stage::Idle {
                    v.toggle_flyout(Flyout::Input);
                    cx.notify();
                }
            })),
        );
        bar = bar.child(separator(p));
        // 录制按钮：待录为空心圆+红点，录制中为红色方块。
        let record = button_shell(
            "rec-record",
            output_state::record_button_hint(self.stage),
            !finishing,
            p,
        )
        .child(if recording {
            toolbar_icons::action(Icon::Stop, RED)
                .size(px(18.))
                .into_any_element()
        } else {
            div()
                .relative()
                .size(px(18.))
                .child(toolbar_icons::action(Icon::Record, p.foreground).size(px(18.)))
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .child(toolbar_icons::action(Icon::Dot, RED).size(px(18.))),
                )
                .into_any_element()
        })
        .on_click(cx.listener(|v, _, _, cx| {
            v.toggle_record(cx);
            cx.notify();
        }));
        bar = bar.child(record);
        bar = bar.child(
            icon_button(
                "rec-pause",
                if paused { Icon::Play } else { Icon::Pause },
                if paused { PAUSED_COLOR } else { p.foreground },
                if paused {
                    tr!("继续 · F10", "Resume · F10")
                } else {
                    tr!("暂停 · F10", "Pause · F10")
                },
                recording && !finishing,
                p,
            )
            .on_click(cx.listener(|v, _, _, cx| {
                v.toggle_pause();
                cx.notify();
            })),
        );
        let drawing = self.drawing_active(cx);
        bar = bar.child(
            toggle_button(
                "rec-draw",
                Icon::Markup,
                if drawing {
                    tr!("收起标注 · Esc", "Close markup · Esc")
                } else {
                    tr!("标注", "Markup")
                },
                recording && !finishing,
                drawing,
                p,
            )
            .on_click(cx.listener(|v, _, _, cx| v.toggle_drawing(cx))),
        );
        let hidden = self.annotations_hidden();
        let has_annotations = self
            .drawing
            .and_then(|h| h.read(cx).ok().map(|v| v.has_annotations()))
            .unwrap_or(false);
        bar = bar.child(
            toggle_button(
                "rec-eye",
                if hidden { Icon::EyeOff } else { Icon::Eye },
                if hidden {
                    tr!("显示标注", "Show markup")
                } else {
                    tr!(
                        "隐藏标注 · 录像里也不显示",
                        "Hide markup · also hidden in the video"
                    )
                },
                recording && !finishing && (has_annotations || hidden),
                hidden,
                p,
            )
            .on_click(cx.listener(|v, _, _, cx| {
                v.toggle_annotations_hidden(cx);
                cx.notify();
            })),
        );
        bar = bar.child(div().flex_1());
        bar = bar.child(separator(p));
        bar.child(
            icon_button(
                "rec-close",
                Icon::Close,
                p.foreground,
                if recording {
                    tr!("停止并关闭 · 存入历史", "Stop and close · kept in history")
                } else {
                    tr!("关闭 · Esc", "Close · Esc")
                },
                true,
                p,
            )
            .on_click(cx.listener(|v, _, w, cx| v.close(w, cx))),
        )
    }

    /// 标注工具栏：工具｜颜色和粗细｜撤销、重做、删除、清空｜完成。录制中展开，贴在主工具栏外侧并右对齐。
    fn annotation_bar(&self, p: Palette, cx: &mut Context<Self>) -> gpui::Div {
        let state = self.drawing.and_then(|h| {
            h.read(cx)
                .ok()
                .map(|v| (v.tool(), v.style(), v.edit_state(), v.has_annotations()))
        });
        let Some((tool, style, (can_undo, can_redo, selected), any)) = state else {
            return div();
        };
        let mut bar = div()
            .w(px(annotation_width()))
            .h(px(ANNOTATION_HEIGHT))
            .px(px(6.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(2.))
            .rounded_lg()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .text_color(rgb(p.foreground));
        for (id, live, icon, hint) in [
            (
                "ann-select",
                LiveTool::Select,
                Icon::Edit,
                tr!(
                    "编辑 · 选中后可移动、删除",
                    "Edit · select to move or delete"
                ),
            ),
            (
                "ann-rect",
                LiveTool::Rectangle,
                Icon::Rectangle,
                LiveTool::Rectangle.tool().label(),
            ),
            (
                "ann-arrow",
                LiveTool::Arrow,
                Icon::Arrow,
                LiveTool::Arrow.tool().label(),
            ),
            (
                "ann-pen",
                LiveTool::Pen,
                Icon::Pen,
                LiveTool::Pen.tool().label(),
            ),
            (
                "ann-highlight",
                LiveTool::Highlight,
                Icon::Highlighter,
                LiveTool::Highlight.tool().label(),
            ),
        ] {
            bar = bar.child(
                toggle_button(id, icon, hint, true, tool == live, p).on_click(cx.listener(
                    move |v, _, _, cx| {
                        v.with_drawing(cx, |d, cx| d.choose(live, cx));
                        cx.notify();
                    },
                )),
            );
        }
        bar = bar.child(separator(p));
        let color = style.map_or(SWATCHES[0], |(_, s)| s.color);
        let opacity = style.map_or(1., |(_, s)| s.opacity.max(0.35));
        bar = bar.child(
            button_shell(
                "ann-style",
                tr!("颜色和粗细", "Color and width"),
                style.is_some(),
                p,
            )
            .when(self.flyout == Some(Flyout::Style), |d| {
                d.bg(rgb(p.selected))
            })
            .child(
                div()
                    .size(px(16.))
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(p.border))
                    .bg(rgb(color))
                    .opacity(opacity),
            )
            .on_click(cx.listener(move |v, _, _, cx| {
                if style.is_some() {
                    v.toggle_flyout(Flyout::Style);
                    cx.notify();
                }
            })),
        );
        bar = bar.child(separator(p));
        for (id, icon, hint, enabled, action) in [
            (
                "ann-undo",
                Icon::Undo,
                tr!("撤销 · Ctrl+Z", "Undo · Ctrl+Z"),
                can_undo,
                recording::live_annotation::EditKey::Undo,
            ),
            (
                "ann-redo",
                Icon::Redo,
                tr!("重做 · Ctrl+Y", "Redo · Ctrl+Y"),
                can_redo,
                recording::live_annotation::EditKey::Redo,
            ),
            (
                "ann-delete",
                Icon::ObjectEraser,
                tr!("删除选中 · Delete", "Delete selected · Delete"),
                selected,
                recording::live_annotation::EditKey::Delete,
            ),
            (
                "ann-clear",
                Icon::Eraser,
                tr!("清空 · Shift+Delete", "Clear · Shift+Delete"),
                any,
                recording::live_annotation::EditKey::Clear,
            ),
        ] {
            bar = bar.child(
                icon_button(id, icon, p.foreground, hint, enabled, p).on_click(cx.listener(
                    move |v, _, _, cx| {
                        if !enabled {
                            return;
                        }
                        v.with_drawing(cx, |d, cx| match action {
                            recording::live_annotation::EditKey::Undo => d.undo(cx),
                            recording::live_annotation::EditKey::Redo => d.redo(cx),
                            recording::live_annotation::EditKey::Delete => d.delete_selected(cx),
                            recording::live_annotation::EditKey::Clear => d.clear(cx),
                        });
                        cx.notify();
                    },
                )),
            );
        }
        bar = bar.child(separator(p));
        bar = bar.child(
            icon_button(
                "ann-done",
                Icon::Confirm,
                p.foreground,
                tr!("完成标注 · Esc", "Done · Esc"),
                true,
                p,
            )
            .on_click(cx.listener(|v, _, _, cx| v.toggle_drawing(cx))),
        );
        div().w_full().flex().justify_end().child(bar)
    }

    /// 颜色、粗细、透明度弹层：点一下即生效，弹层保持打开便于连续调整。
    fn style_panel(&self, p: Palette, cx: &mut Context<Self>) -> gpui::Div {
        let style = self
            .drawing
            .and_then(|h| h.read(cx).ok().and_then(|v| v.style()));
        let Some((tool, style)) = style else {
            return div();
        };
        let highlight = tool == crate::annotation::Tool::Highlighter;
        let widths: [f32; 4] = if highlight {
            [8., 14., 20., 28.]
        } else {
            [2., 4., 6., 10.]
        };
        let opacities: [f32; 4] = if highlight {
            [0.2, 0.35, 0.5, 0.65]
        } else {
            [1., 0.75, 0.5, 0.25]
        };
        let apply = |color: Option<u32>, width: Option<f32>, opacity: Option<f32>| {
            cx.listener(move |v: &mut Self, _: &gpui::ClickEvent, _, cx| {
                v.with_drawing(cx, |d, cx| {
                    d.apply_style(color, width, opacity, cx);
                });
                cx.notify();
            })
        };
        let mut swatches = div().flex().gap(px(6.));
        for (i, color) in SWATCHES.into_iter().enumerate() {
            let selected = style.color == color;
            swatches = swatches.child(
                div()
                    .id(("ann-swatch", i))
                    .size(px(SWATCH))
                    .rounded_full()
                    .cursor_pointer()
                    .bg(rgb(color))
                    .when(selected, |d| d.border_2().border_color(rgb(p.accent)))
                    .when(!selected, |d| d.border_1().border_color(rgb(p.border)))
                    .on_click(apply(Some(color), None, None)),
            );
        }
        let label = |text: &'static str| {
            div()
                .w(px(44.))
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(rgb(p.muted))
                .child(text)
        };
        let mut width_row = div()
            .h(px(24.))
            .flex()
            .items_center()
            .gap(px(2.))
            .child(label(tr!("粗细", "Width")));
        for (i, width) in widths.into_iter().enumerate() {
            let selected = (style.width - width).abs() < 0.01;
            // 圆点直径示意粗细，最大 14。
            let dot = if highlight {
                [6., 9., 12., 14.][i]
            } else {
                (width + 2.).min(14.)
            };
            width_row = width_row.child(
                div()
                    .id(("ann-width", i))
                    .w(px(28.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(rgb(p.selected)))
                    .hover(move |s| s.bg(rgb(p.hover)))
                    .child(div().size(px(dot)).rounded_full().bg(rgb(if selected {
                        p.accent
                    } else {
                        p.foreground
                    })))
                    .on_click(apply(None, Some(width), None)),
            );
        }
        let mut opacity_row = div()
            .h(px(24.))
            .flex()
            .items_center()
            .gap(px(2.))
            .child(label(tr!("透明度", "Opacity")));
        for (i, opacity) in opacities.into_iter().enumerate() {
            let selected = (style.opacity - opacity).abs() < 0.005;
            opacity_row = opacity_row.child(
                div()
                    .id(("ann-opacity", i))
                    .min_w(px(28.))
                    .h(px(24.))
                    .px(px(4.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(rgb(if selected { p.accent } else { p.foreground }))
                    .when(selected, |d| d.bg(rgb(p.selected)))
                    .hover(move |s| s.bg(rgb(p.hover)))
                    .child(format!("{:.0}%", opacity * 100.))
                    .on_click(apply(None, None, Some(opacity))),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(swatches)
            .child(width_row)
            .child(opacity_row)
    }

    /// 预览工具栏：尺寸、播放、时间、倍速、文件信息｜混音、GIF、保存、复制、历史、重录｜完成。宽度随内容。
    fn preview_bar(&self, p: Palette, window: &Window, cx: &mut Context<Self>) -> (gpui::Div, f32) {
        let playback = self.preview_playback_state(cx);
        let playback_ready = playback == PreviewPlaybackState::Ready && !self.closing;
        let (playing, position, duration, rate, _) = self.player_state(cx);
        let size = self.result.as_ref().map_or((0, 0), |r| (r.width, r.height));
        let time_label = match playback {
            PreviewPlaybackState::Opening => tr!("正在打开预览…", "Opening preview…").to_string(),
            PreviewPlaybackState::Unavailable => tr!("预览不可用", "Preview unavailable").to_string(),
            PreviewPlaybackState::Ready => format!(
                "{} / {}",
                format_clock(position),
                format_clock(duration)
            ),
        };
        let time_width = label_width(Some(window), &time_label, 12.).max(78.);
        let ratio = if duration.is_zero() {
            0.
        } else {
            (position.as_secs_f32() / duration.as_secs_f32()).clamp(0., 1.)
        };
        let width = self.width();
        let seek = div()
            .id("preview-seek")
            .h(px(SEEK_HEIGHT))
            .mx(px(12.))
            .flex()
            .items_center()
            .opacity(if playback_ready { 1. } else { 0.35 })
            .when(playback_ready, |seek| seek.cursor_pointer())
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(4.))
                    .rounded_full()
                    .bg(rgb(p.border))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .h_full()
                            .w(px((width - 24.) * ratio))
                            .rounded_full()
                            .bg(rgb(p.accent)),
                    ),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |v, e: &MouseDownEvent, _, cx| {
                    if !v.preview_playback_ready(cx) {
                        v.seeking = false;
                        return;
                    }
                    v.seeking = true;
                    let ratio = (f32::from(e.position.x) - 12.) / (width - 24.);
                    v.with_player(cx, |p| p.seek_ratio(ratio));
                }),
            )
            .on_mouse_move(cx.listener(move |v, e: &MouseMoveEvent, _, cx| {
                if !v.preview_playback_ready(cx) {
                    v.seeking = false;
                    return;
                }
                if v.seeking && e.pressed_button == Some(MouseButton::Left) {
                    let ratio = (f32::from(e.position.x) - 12.) / (width - 24.);
                    v.with_player(cx, |p| p.seek_ratio(ratio));
                } else {
                    v.seeking = false;
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|v, _, _, _| v.seeking = false),
            );
        let output = self.output.clone();
        let is_gif = output
            .as_ref()
            .and_then(|p| p.extension())
            .is_some_and(|e| e.eq_ignore_ascii_case("gif"));
        let bytes = output
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or(0, |m| m.len());
        let quality = self.result.as_ref().map_or(tr!("标准", "Standard"), |r| {
            if is_gif && r.preset != Preset::Gif {
                tr!("标准", "Standard")
            } else {
                r.preset.quality_label()
            }
        });
        let info = match &self.gif_job {
            Some(job) => trf!(
                "GIF 转换 {:.0}%",
                "GIF conversion {:.0}%",
                f32::from_bits(job.progress.load(Ordering::Relaxed)) * 100.
            ),
            None => format!(
                "{} · {} · {}",
                if is_gif { "GIF" } else { "MP4" },
                quality,
                format_bytes(bytes)
            ),
        };
        let encoding_info_enabled = crate::file_launch_command::experimental_media_enabled();
        let info = if encoding_info_enabled {
            format!("{info} ▾")
        } else {
            info
        };
        // 文件名一行，所在文件夹第二行（次要色）。
        let info_hint = output.map_or_else(
            || tr!("录制信息", "Recording info").to_string(),
            |path| {
                let text = display_path_text(&path.to_string_lossy());
                match text.rsplit_once('\\') {
                    Some((folder, name)) => format!("{name}\n{folder}"),
                    None => text,
                }
            },
        );
        let can_gif = !is_gif;
        let mut row = RowWidth::default();
        let mut bar = div()
            .h(px(BAR_HEIGHT))
            .px(px(6.))
            .flex()
            .items_center()
            .gap(px(2.));
        bar = bar.child(grip(p));
        row.add(14.);
        let dimensions = format!("{}×{}", size.0, size.1);
        row.add(8. + label_width(Some(window), &dimensions, 11.));
        bar = bar.child(
            div()
                .px(px(4.))
                .flex_shrink_0()
                .text_size(px(11.))
                .text_color(rgb(p.muted))
                .child(dimensions),
        );
        bar = bar.child(
            icon_button(
                "preview-play",
                if playing { Icon::Pause } else { Icon::Play },
                p.foreground,
                if playing {
                    tr!("暂停 · 空格", "Pause · Space")
                } else {
                    tr!("播放 · 空格", "Play · Space")
                },
                playback_ready,
                p,
            )
            .on_click(cx.listener(|v, _, _, cx| {
                if v.preview_playback_ready(cx) {
                    v.with_player(cx, |p| p.toggle());
                    cx.notify();
                }
            })),
        );
        row.add(30.);
        bar = bar.child(
            div()
                .min_w(px(time_width))
                .flex_shrink_0()
                .text_size(px(12.))
                .whitespace_nowrap()
                .when(playback == PreviewPlaybackState::Ready, |label| {
                    label.font_family("Consolas")
                })
                .child(time_label),
        );
        row.add(time_width);
        if playback == PreviewPlaybackState::Unavailable {
            let label = tr!("重试预览", "Retry preview");
            let enabled = !self.closing
                && !self.picking_save
                && self.mix_job.is_none()
                && self.gif_job.is_none()
                && self.result.is_some();
            row.add(12. + label_width(Some(window), label, 12.));
            bar = bar.child(
                chip(
                    "preview-retry",
                    label,
                    tr!("重新打开本次录制的预览", "Reopen this recording's preview"),
                    enabled,
                    false,
                    p,
                )
                .on_click(cx.listener(|v, _, w, cx| {
                    v.retry_result_preview(w, cx);
                    cx.notify();
                })),
            );
        }
        let rate_label = format!("{}×", trim_rate(rate));
        row.add(12. + label_width(Some(window), &rate_label, 12.).max(RATE_MIN_WIDTH));
        bar = bar.child(
            chip(
                "preview-rate",
                div()
                    .min_w(px(RATE_MIN_WIDTH))
                    .flex()
                    .justify_center()
                    .child(rate_label),
                tr!("播放速度", "Playback speed"),
                playback_ready,
                self.flyout == Some(Flyout::Rate),
                p,
            )
            .on_click(cx.listener(|v, _, _, cx| {
                if v.preview_playback_ready(cx) {
                    v.toggle_flyout(Flyout::Rate);
                    cx.notify();
                }
            })),
        );
        row.add((label_width(Some(window), &info, 11.) + 16.).max(INFO_MIN_WIDTH) + 2.);
        let info_flash = self.flashing("preview-info") || self.flyout == Some(Flyout::EncodingInfo);
        bar = bar.child(with_tip(
            div()
                .id("preview-info")
                .h(px(24.))
                .min_w(px(INFO_MIN_WIDTH))
                .px(px(8.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded_md()
                .border_1()
                .border_color(rgb(if info_flash { p.accent } else { p.border }))
                .text_size(px(11.))
                .text_color(rgb(if info_flash { p.accent } else { p.muted }))
                .whitespace_nowrap()
                .when(encoding_info_enabled, |info| info.cursor_pointer())
                .child(info)
                .on_click(cx.listener(|view, _, window, cx| {
                    if view.stage == Stage::Preview
                        && crate::file_launch_command::experimental_media_enabled()
                    {
                        view.toggle_flyout(Flyout::EncodingInfo);
                        window.focus(&view.focus);
                        cx.notify();
                    }
                })),
            "preview-info",
            info_hint.into(),
            true,
        ));
        bar = bar.child(separator(p));
        row.add(7.);
        let has_audio = self
            .preview
            .and_then(|p| {
                p.read(cx)
                    .ok()
                    .map(|v| !v.player.audio_sources().is_empty())
            })
            .unwrap_or(false);
        if has_audio {
            let initial_mix = self
                .result
                .as_ref()
                .map(|result| result.initial_mix)
                .unwrap_or_default();
            let adjusted = self.levels != initial_mix;
            bar = bar.child(
                icon_button(
                    "preview-mix",
                    Icon::Volume,
                    if adjusted { p.accent } else { p.foreground },
                    tr!("音量混合", "Volume mix"),
                    !self.closing,
                    p,
                )
                .on_click(cx.listener(|v, _, _, cx| {
                    v.toggle_flyout(Flyout::Mix);
                    cx.notify();
                })),
            );
            row.add(30.);
        }
        if can_gif {
            let label = if self.gif_job.is_some() {
                tr!("取消", "Cancel")
            } else {
                "GIF"
            };
            row.add(12. + label_width(Some(window), label, 12.));
            bar = bar.child(
                chip(
                    "preview-gif",
                    label,
                    if self.gif_job.is_some() {
                        tr!("取消 GIF 转换", "Cancel GIF conversion")
                    } else {
                        tr!("转为 GIF", "Convert to GIF")
                    },
                    !self.closing,
                    self.gif_job.is_some(),
                    p,
                )
                .on_click(cx.listener(|v, _, _, cx| {
                    v.convert_gif();
                    cx.notify();
                })),
            );
        }
        let ready = self.preview_output_ready();
        let flash_icon = |id: &str, icon: Icon| {
            if self.flashing(id) {
                (Icon::Confirm, p.accent)
            } else {
                (icon, p.foreground)
            }
        };
        let (icon, color) = flash_icon("preview-save", Icon::Save);
        bar = bar.child(
            icon_button(
                "preview-save",
                icon,
                color,
                tr!("保存 · Ctrl+S", "Save · Ctrl+S"),
                ready,
                p,
            )
            .on_click(cx.listener(move |v, _, w, cx| {
                if ready {
                    v.save_as(w, cx);
                }
            })),
        );
        let (icon, color) = flash_icon("preview-copy", Icon::Copy);
        bar = bar.child(
            icon_button(
                "preview-copy",
                icon,
                color,
                tr!("复制文件 · Ctrl+C", "Copy file · Ctrl+C"),
                ready,
                p,
            )
            .on_click(cx.listener(move |v, _, w, cx| {
                if ready {
                    v.copy_file(w);
                    cx.notify();
                }
            })),
        );
        bar = bar.child(
            icon_button(
                "preview-history",
                Icon::History,
                p.foreground,
                tr!("录屏历史", "Recording history"),
                true,
                p,
            )
            .on_click(cx.listener(|v, _, _, cx| {
                v.open_history(cx);
                cx.notify();
            })),
        );
        bar = bar.child(
            icon_button(
                "preview-rerecord",
                Icon::Reselect,
                p.foreground,
                tr!("重新录制 · 会删除本次", "Record again · deletes this one"),
                !self.closing,
                p,
            )
            .on_click(cx.listener(|v, _, _, cx| {
                v.rerecord(cx);
                cx.notify();
            })),
        );
        for width in [30., 30., 30., 30., 0., 7., 30.] {
            row.add(width);
        }
        bar = bar.child(div().flex_1());
        bar = bar.child(separator(p));
        bar = bar.child(
            icon_button(
                "preview-close",
                Icon::Close,
                p.foreground,
                tr!("完成 · Enter", "Done · Enter"),
                true,
                p,
            )
            .on_click(cx.listener(|v, _, w, cx| v.close(w, cx))),
        );
        (div().flex().flex_col().child(seek).child(bar), row.total())
    }

    /// 按键位置：右侧一块缩略画面，四角与底边中点各一个可点的落点，选中项用强调色。
    fn key_position_row(&self, p: Palette, cx: &mut Context<Self>) -> gpui::Div {
        let enabled = self.options.show_keys;
        let current = self.options.key_position;
        // 缩略画面内宽 38、内高 24；落点 14×10，横向 0／12／24，纵向 0／14。
        let mut map = div()
            .relative()
            .w(px(40.))
            .h(px(26.))
            .flex_shrink_0()
            .rounded_sm()
            .border_1()
            .border_color(rgb(p.muted));
        for (index, position) in KeyPosition::ALL.into_iter().enumerate() {
            let selected = position == current;
            let x = position.align() * 24.;
            let y = if position.top() { 0. } else { 14. };
            map = map.child(
                div()
                    .id(SharedString::from(format!("input-key-pos-{index}")))
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(14.))
                    .h(px(10.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_sm()
                    .when(enabled, |d| {
                        d.cursor_pointer()
                            .hover(move |s| s.bg(rgb(p.hover)))
                            .on_click(cx.listener(move |v, _, _, cx| {
                                v.options.key_position = position;
                                v.save_options();
                                cx.notify();
                            }))
                    })
                    .child(
                        div()
                            .w(px(8.))
                            .h(px(3.))
                            .rounded_sm()
                            .bg(rgb(if selected { p.accent } else { p.muted }))
                            .opacity(if selected { 1. } else { 0.45 }),
                    ),
            );
        }
        div()
            .h(px(28.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(12.))
            .text_color(rgb(p.foreground))
            .opacity(if enabled { 1. } else { 0.4 })
            // 与上方勾选行的文字对齐。
            .child(div().size(px(14.)).flex_shrink_0())
            .child(tr!("按键位置", "Key position"))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(rgb(p.muted))
                    .child(current.label()),
            )
            .child(map)
    }

    fn flyout_panel(&self, flyout: Flyout, p: Palette, cx: &mut Context<Self>) -> gpui::Div {
        let height = flyout.height();
        let left = self.flyout_left(flyout);
        // 纯选项下拉与样式弹层用紧凑内边距；带说明的面板保留原内边距（混音滑块按此定位）。
        let padding = match flyout {
            Flyout::Audio
            | Flyout::OutputDevice
            | Flyout::InputDevice
            | Flyout::Input
            | Flyout::Mix
            | Flyout::AdvancedQuality
            | Flyout::EncodingInfo => 7.,
            Flyout::Style => 6.,
            _ => 4.,
        };
        let mut panel = div()
            .ml(px(left))
            .w(px(self.flyout_width(flyout)))
            .h(px(height))
            .p(px(padding))
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .text_color(rgb(p.foreground));
        match flyout {
            Flyout::EncodingInfo => {
                panel = panel.child(self.encoding_info_panel(p, cx));
            }
            Flyout::AdvancedQuality => {
                panel = panel.child(self.advanced_quality_panel(p, cx));
            }
            Flyout::Style => {
                panel = panel.child(self.style_panel(p, cx));
            }
            Flyout::Delay => {
                panel = panel.child(
                    div()
                        .h(px(22.))
                        .px(px(8.))
                        .text_size(px(11.))
                        .text_color(rgb(p.muted))
                        .child(tr!("录制延迟", "Start delay")),
                );
                for delay in DELAY_CHOICES {
                    panel = panel.child(
                        option_row(
                            format!("delay-{delay}").into(),
                            delay_label(delay),
                            self.options.delay == delay,
                            p,
                        )
                        .on_click(cx.listener(move |v, _, _, cx| {
                            v.options.delay = delay;
                            v.save_options();
                            v.flyout = None;
                            cx.notify();
                        })),
                    );
                }
            }
            Flyout::Fps => {
                for fps in FPS_CHOICES {
                    panel = panel.child(
                        option_row(
                            format!("fps-{fps}").into(),
                            format!("{fps}FPS"),
                            self.options.fps == fps,
                            p,
                        )
                        .when(cfg!(test) && fps == 60, |row| {
                            row.relative().child(bounds_probe("fps-60"))
                        })
                        .on_click(cx.listener(move |v, _, _, cx| {
                            v.options.fps = fps;
                            v.save_options();
                            v.flyout = None;
                            cx.notify();
                        })),
                    );
                }
            }
            Flyout::Preset => {
                for preset in Preset::ALL {
                    panel = panel.child(
                        option_row(
                            format!("preset-{}", preset.label()).into(),
                            preset.label(),
                            self.options.preset == preset,
                            p,
                        )
                        .on_click(cx.listener(move |v, _, _, cx| {
                            if v.stage == Stage::Idle {
                                v.options.preset = preset;
                                v.quality_profile = None;
                                v.save_options();
                                v.flyout = None;
                                cx.notify();
                            }
                        })),
                    );
                }
                if crate::file_launch_command::experimental_media_enabled() {
                    panel = panel.child(
                        div()
                            .h(px(24.))
                            .px(px(8.))
                            .text_size(px(11.))
                            .text_color(rgb(p.muted))
                            .child(tr!("编码方式（实验）", "Encoding (test)")),
                    );
                    for (index, policy) in EncoderPolicy::ALL.into_iter().enumerate() {
                        panel = panel.child(
                            option_row(
                                format!("encoder-policy-{index}").into(),
                                policy.label(),
                                self.encoder_policy == policy,
                                p,
                            )
                            .on_click(cx.listener(
                                move |v, _, _, cx| {
                                    if v.stage == Stage::Idle {
                                        v.encoder_policy = policy;
                                        cx.notify();
                                    }
                                },
                            )),
                        );
                    }
                    panel = panel.child(hint_text(self.encoder_policy.explanation(), p));
                    panel = panel.child(
                        option_row(
                            "rec-quality-options".into(),
                            tr!("画质候选…", "Quality candidates…"),
                            self.quality_profile.is_some() && !self.options.preset.is_gif(),
                            p,
                        )
                        .when(cfg!(test), |row| {
                            row.relative().child(bounds_probe("rec-quality-options"))
                        })
                        .child(div().flex_1())
                        .child("▸")
                        .on_click(cx.listener(|view, _, _, cx| {
                            if view.stage == Stage::Idle
                                && crate::file_launch_command::experimental_media_enabled()
                            {
                                view.flyout = Some(Flyout::Quality);
                                cx.notify();
                            }
                        })),
                    );
                }
            }
            Flyout::Quality => {
                let gif = self.options.preset.is_gif();
                let experimental = crate::file_launch_command::experimental_media_enabled();
                let selected = if experimental && !gif {
                    self.quality_profile
                } else {
                    None
                };
                panel = panel.child(
                    option_row(
                        "rec-quality-current".into(),
                        tr!("使用当前设置", "Use current settings"),
                        selected.is_none(),
                        p,
                    )
                    .flex_shrink_0()
                    .on_click(cx.listener(|view, _, _, cx| {
                        if view.stage == Stage::Idle {
                            view.quality_profile = None;
                            cx.notify();
                        }
                    })),
                );
                if experimental && !gif {
                    for profile in QualityProfile::EXPERIMENTS {
                        panel = panel.child(
                            option_row(
                                format!("rec-quality-{}", profile.id()).into(),
                                profile.label(),
                                selected == Some(profile),
                                p,
                            )
                            .flex_shrink_0()
                            .on_click(cx.listener(
                                move |view, _, _, cx| {
                                    if view.stage == Stage::Idle
                                        && crate::file_launch_command::experimental_media_enabled()
                                        && !view.options.preset.is_gif()
                                    {
                                        view.quality_profile = Some(profile);
                                        cx.notify();
                                    }
                                },
                            )),
                        );
                    }
                }
                if experimental {
                    panel = panel.child(
                        option_row(
                            "rec-quality-advanced".into(),
                            tr!("高级请求…", "Advanced requests…"),
                            self.quality_editor.draft.enabled && !gif,
                            p,
                        )
                        .when(cfg!(test), |row| {
                            row.relative().child(bounds_probe("rec-quality-advanced"))
                        })
                        .flex_shrink_0()
                        .child(div().flex_1())
                        .child("▸")
                        .on_click(cx.listener(|view, _, _, cx| {
                            if view.stage == Stage::Idle
                                && crate::file_launch_command::experimental_media_enabled()
                            {
                                view.flyout = Some(Flyout::AdvancedQuality);
                                cx.notify();
                            }
                        })),
                    );
                    if self.last_encoding_session.is_some()
                        || self
                            .result
                            .as_ref()
                            .is_some_and(|result| result.encoding.is_some())
                    {
                        panel = panel.child(
                            option_row(
                                "rec-quality-encoding-info".into(),
                                tr!("上次编码信息…", "Last encoding info…"),
                                false,
                                p,
                            )
                            .flex_shrink_0()
                            .child(div().flex_1())
                            .child("▸")
                            .on_click(cx.listener(
                                |view, _, window, cx| {
                                    if view.stage == Stage::Idle
                                        && crate::file_launch_command::experimental_media_enabled()
                                    {
                                        view.flyout = Some(Flyout::EncodingInfo);
                                        window.focus(&view.focus);
                                        cx.notify();
                                    }
                                },
                            )),
                        );
                    }
                }
                let explanation = if gif {
                    tr!(
                        "GIF 使用当前设置，画质候选仅适用于 MP4。",
                        "GIF uses current settings. Quality candidates apply to MP4 only."
                    )
                } else {
                    selected
                        .unwrap_or(QualityProfile::Current(self.options.preset))
                        .explanation()
                };
                panel = panel.child(
                    div()
                        .id("rec-quality-description")
                        .flex_1()
                        .min_h(px(0.))
                        .min_w(px(0.))
                        .overflow_y_scroll()
                        .child(hint_text(explanation, p).w_full().min_w(px(0.))),
                );
            }
            Flyout::Audio => {
                let gif = self.options.preset.is_gif();
                panel = panel
                    .child(
                        check_row(
                            "audio-mic",
                            tr!("录制麦克风音频", "Record microphone audio"),
                            self.options.microphone,
                            !gif,
                            p,
                        )
                        .on_click(cx.listener(move |v, _, _, cx| {
                            if !gif {
                                v.stop_audio_input_check();
                                v.options.microphone = !v.options.microphone;
                                v.save_options();
                                cx.notify();
                            }
                        })),
                    )
                    .child(
                        check_row(
                            "audio-system",
                            tr!("录制系统音频", "Record system audio"),
                            self.options.system_audio,
                            !gif,
                            p,
                        )
                        .on_click(cx.listener(move |v, _, _, cx| {
                            if !gif {
                                v.stop_audio_input_check();
                                v.options.system_audio = !v.options.system_audio;
                                v.save_options();
                                cx.notify();
                            }
                        })),
                    )
                    .when(crate::file_launch_command::experimental_media_enabled(), |panel| panel
                        .child(self.audio_device_selector(EndpointKind::Render, p, cx))
                        .child(self.audio_device_selector(EndpointKind::Capture, p, cx))
                        .child(self.audio_initial_gain_panel(p, cx))
                        .child(self.audio_input_check_panel(p, cx)))
                    .child(hint_text(
                        if gif {
                            tr!("GIF 格式不支持音频录制。", "GIF doesn't support audio.")
                        } else if crate::file_launch_command::experimental_media_enabled() {
                            tr!("设备选择在下次录制生效，音轨编码为 AAC。", "Device choices apply to the next recording; audio uses AAC.")
                        } else {
                            tr!("使用系统默认输出与输入设备，音轨编码为 AAC。", "Uses the system default output and input devices; audio is encoded as AAC.")
                        },
                        p,
                    ));
            }
            Flyout::OutputDevice => {
                panel = self.audio_device_panel(panel, EndpointKind::Render, p, cx)
            }
            Flyout::InputDevice => {
                panel = self.audio_device_panel(panel, EndpointKind::Capture, p, cx)
            }
            Flyout::Input => {
                panel = panel
                    .child(
                        check_row("input-keys", tr!("显示按键", "Show keystrokes"), self.options.show_keys, true, p).on_click(
                            cx.listener(|v, _, _, cx| {
                                v.options.show_keys = !v.options.show_keys;
                                v.save_options();
                                cx.notify();
                            }),
                        ),
                    )
                    .child(
                        check_row("input-mouse", tr!("显示鼠标点击和滚轮", "Show mouse clicks and wheel"), self.options.show_mouse, true, p)
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.options.show_mouse = !v.options.show_mouse;
                                v.save_options();
                                cx.notify();
                            })),
                    )
                    .child(
                        check_row(
                            "input-filter",
                            tr!("过滤打字按键", "Filter typing keys"),
                            self.options.filter_typing,
                            self.options.show_keys,
                            p,
                        )
                        .on_click(cx.listener(|v, _, _, cx| {
                            if v.options.show_keys {
                                v.options.filter_typing = !v.options.filter_typing;
                                v.save_options();
                                cx.notify();
                            }
                        })),
                    )
                    .child(self.key_position_row(p, cx))
                    .child(
                        check_row("input-cursor", tr!("录制鼠标指针", "Record mouse pointer"), self.options.show_cursor, true, p)
                            .on_click(cx.listener(|v, _, _, cx| {
                                v.options.show_cursor = !v.options.show_cursor;
                                v.save_options();
                                cx.notify();
                            })),
                    )
                    .child(hint_text(
                        tr!("默认隐藏字母/主键盘数字等打字键；快捷键、空格、方向键、小键盘仍显示，连续相同按键合并为 ×N。密码框内不显示。", "Hides typing keys such as letters and main-keyboard digits by default; shortcuts, Space, arrow keys and the numpad still show, and repeated keys merge as ×N. Nothing shows in password fields."),
                        p,
                    ));
            }
            Flyout::Mix => {
                let sources = self
                    .preview
                    .and_then(|p| p.read(cx).ok().map(|v| v.player.audio_sources()))
                    .unwrap_or_default();
                for source in sources {
                    let (label, percent) = match source {
                        AudioSource::System => {
                            (tr!("系统音频", "System audio"), self.levels.system)
                        }
                        AudioSource::Microphone => {
                            (tr!("麦克风", "Microphone"), self.levels.microphone)
                        }
                    };
                    let ratio = percent as f32 / MixLevels::MAX as f32;
                    panel = panel
                        .child(
                            div()
                                .h(px(20.))
                                .px(px(8.))
                                .flex()
                                .items_end()
                                .text_size(px(11.))
                                .text_color(rgb(p.muted))
                                .child(label),
                        )
                        .child(
                            div()
                                .id(match source {
                                    AudioSource::System => "mix-system",
                                    AudioSource::Microphone => "mix-mic",
                                })
                                .h(px(28.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .cursor_pointer()
                                .child(
                                    div()
                                        .relative()
                                        .w(px(SLIDER_WIDTH))
                                        .h(px(4.))
                                        .rounded_full()
                                        .bg(rgb(p.border))
                                        .child(
                                            div()
                                                .absolute()
                                                .left_0()
                                                .top_0()
                                                .h_full()
                                                .w(px(SLIDER_WIDTH * ratio))
                                                .rounded_full()
                                                .bg(rgb(p.accent)),
                                        )
                                        .child(
                                            div()
                                                .absolute()
                                                .top(px(-5.))
                                                .left(px(SLIDER_WIDTH * ratio - 7.))
                                                .size(px(14.))
                                                .rounded_full()
                                                .bg(rgb(p.background))
                                                .border_2()
                                                .border_color(rgb(p.accent)),
                                        ),
                                )
                                .child(
                                    div()
                                        .min_w(px(40.))
                                        .text_size(px(11.))
                                        .child(format!("{percent}%")),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |v, e: &MouseDownEvent, _, cx| {
                                        v.mix_drag = Some(source);
                                        v.set_level(source, f32::from(e.position.x), cx);
                                        cx.notify();
                                    }),
                                ),
                        );
                }
                panel = panel
                    .child(hint_text(tr!("预览即时生效；保存、复制或完成时按此音量写入 MP4。", "Applies to the preview immediately; saving, copying or finishing writes the MP4 at this volume."), p))
                    .on_mouse_move(cx.listener(|v, e: &MouseMoveEvent, _, cx| {
                        if let Some(source) = v.mix_drag {
                            if e.pressed_button == Some(MouseButton::Left) {
                                v.set_level(source, f32::from(e.position.x), cx);
                                cx.notify();
                            } else {
                                v.mix_drag = None;
                            }
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|v, _, _, _| v.mix_drag = None),
                    );
            }
            Flyout::Rate => {
                if self.preview_playback_ready(cx) {
                    let (_, _, _, current, _) = self.player_state(cx);
                    for rate in RATES {
                        panel = panel.child(
                            option_row(
                                format!("rate-{rate}").into(),
                                format!("{}×", trim_rate(rate)),
                                (current - rate).abs() < 0.01,
                                p,
                            )
                            .on_click(cx.listener(move |v, _, _, cx| {
                                if v.preview_playback_ready(cx) {
                                    v.with_player(cx, |p| p.set_rate(rate));
                                }
                                v.flyout = None;
                                cx.notify();
                            })),
                        );
                    }
                } else {
                    panel = panel.child(hint_text(
                        tr!("预览尚不可播放", "Preview is not ready"),
                        p,
                    ));
                }
            }
        }
        panel
    }
}

fn trim_rate(rate: f32) -> String {
    let text = format!("{rate:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn grip(p: Palette) -> gpui::Stateful<gpui::Div> {
    div()
        .id("rec-grip")
        .group("rec-grip")
        .w(px(14.))
        .h(px(30.))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .rounded_md()
        .cursor(gpui::CursorStyle::ResizeAll)
        .hover(move |s| s.bg(rgb(p.hover)))
        .child(
            toolbar_icons::action(Icon::Grip, p.muted)
                .w(px(10.))
                .h(px(20.))
                .group_hover("rec-grip", move |s| s.text_color(rgb(p.foreground))),
        )
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            cx.stop_propagation();
            window.start_window_move();
        })
}

fn separator(p: Palette) -> gpui::Div {
    div()
        .w(px(1.))
        .h(px(18.))
        .mx(px(3.))
        .flex_shrink_0()
        .bg(rgb(p.border))
}

impl Render for RecordingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.quality_editor.refresh_context(
            self.region,
            &self.options,
            self.quality_profile,
            crate::file_launch_command::experimental_media_enabled(),
        );
        // 被隐藏的高级输入框不能继续持有录屏快捷键；输入中仍由子控件处理组合键。
        if self.flyout != Some(Flyout::AdvancedQuality) && self.quality_editor.has_focus(window, cx)
        {
            window.focus(&self.focus);
        }
        if let Some(opened) = self.trace_opened.take() {
            trace_open("first render", opened);
        }
        if std::mem::take(&mut self.resize_handles_pending) {
            cx.on_next_frame(window, |v, _, cx| {
                v.create_resize_handles();
                cx.notify();
            });
        }
        crate::theme::sync_window(window, cx);
        let p = crate::theme::palette(cx);
        self.light = crate::theme::is_light(cx);
        self.annotation_open = self.stage == Stage::Recording && self.drawing_active(cx);
        if self.stage == Stage::Preview
            && self.preview_playback_state(cx) != PreviewPlaybackState::Ready
        {
            // 异步预览失败可能没有最后一次鼠标事件，清理拖动状态以免阻塞输出。
            self.seeking = false;
            if self.flyout == Some(Flyout::Rate) {
                self.flyout = None;
            }
        }
        let (bar, width) = if self.stage == Stage::Preview {
            self.preview_bar(p, window, cx)
        } else {
            (
                self.transport_bar(p, cx),
                transport_width(Some(&*window), &self.options),
            )
        };
        // 下一次轮询按此宽度重新定位窗口；标注工具栏比主栏宽时以它为准。
        self.bar_width = if self.annotation_open {
            width.max(annotation_width())
        } else {
            width
        };
        let bar = div()
            .id("recording-toolbar")
            .w_full()
            .h(px(self.bar_height()))
            .flex_shrink_0()
            .rounded_lg()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .text_color(rgb(p.foreground))
            .child(bar);
        // 主栏外侧依次是标注工具栏、下拉面板；说明和消息是独立浮层，不占窗口。
        let mut extras = Vec::new();
        if self.annotation_open {
            extras.push(self.annotation_bar(p, cx).into_any_element());
        }
        if let Some(flyout) = self.flyout {
            extras.push(self.flyout_panel(flyout, p, cx).into_any_element());
        }
        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(4.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|v, e: &KeyDownEvent, w, cx| v.on_key(e, w, cx)));
        if self.flyout_above {
            extras.reverse();
            root = root.justify_end().children(extras).child(bar);
        } else {
            root = root.child(bar).children(extras);
        }
        root
    }
}

/// 开始前倒计时：选区中央的大号数字，排除采集。
pub struct CountdownView {
    until: Instant,
}
impl CountdownView {
    fn open(region: ScreenRect, seconds: u32, cx: &mut App) -> Result<WindowHandle<Self>> {
        let scale = crate::input_overlay::region_scale(region);
        let (w, h) = ((168. * scale) as u32, (112. * scale) as u32);
        let placement = Placement {
            x: region.left + (region.width as i32 - w as i32) / 2,
            y: region.top + (region.height as i32 - h as i32) / 2,
            width: w,
            height: h,
        };
        let (display_id, bounds) = crate::pin_native::initial_pin_bounds(placement, cx);
        let until = Instant::now() + Duration::from_secs(u64::from(seconds));
        Ok(cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                display_id,
                kind: gpui::WindowKind::PopUp,
                window_background: gpui::WindowBackgroundAppearance::Transparent,
                focus: false,
                ..Default::default()
            },
            move |window, cx| {
                let _ = native_setup(hwnd(window), placement);
                let view = cx.new(|_| Self { until });
                let weak = view.downgrade();
                cx.spawn(async move |cx| loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(100))
                        .await;
                    if weak.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                })
                .detach();
                view
            },
        )?)
    }
}
impl Render for CountdownView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let left = self.until.saturating_duration_since(Instant::now());
        let seconds = left.as_secs() + u64::from(left.subsec_millis() > 0);
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .rounded_lg()
            .bg(rgba(0x000000d9))
            .text_color(rgb(0xffffff))
            .child(
                div()
                    .text_size(px(48.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(seconds.max(1).to_string()),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .text_color(rgba(0xffffffe6))
                    .child(tr!("即将开始录屏", "Recording starts soon")),
            )
    }
}

fn virtual_bounds() -> ScreenRect {
    unsafe {
        ScreenRect {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32,
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32,
        }
    }
}

fn monitor_work(point: (i32, i32)) -> Placement {
    unsafe {
        let monitor = MonitorFromPoint(
            POINT {
                x: point.0,
                y: point.1,
            },
            MONITOR_DEFAULTTONEAREST,
        );
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let r = info.rcWork;
            Placement {
                x: r.left,
                y: r.top,
                width: (r.right - r.left).max(1) as u32,
                height: (r.bottom - r.top).max(1) as u32,
            }
        } else {
            Placement {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }
        }
    }
}

/// 工具栏贴靠选区右下外侧；下方放不下时放上方，再不行放在选区内底部。
/// 弹出面板优先向下展开，超出工作区时改为向上，工具栏本身不跳动。
fn toolbar_placement(
    region: ScreenRect,
    anchor: Option<(i32, i32)>,
    width: f32,
    bar: f32,
    extra: f32,
    scale: f32,
) -> (Placement, bool) {
    let right = region.left + region.width as i32;
    let bottom = region.top + region.height as i32;
    let work = monitor_work((
        region.left + region.width as i32 / 2,
        bottom.saturating_sub(1),
    ));
    let w = ((width * scale).round() as u32).min(work.width);
    let bar = (bar * scale).round() as i32;
    let extra = (extra * scale).round() as i32;
    let gap = (6. * scale).round() as i32;
    let work_right = work.x + work.width as i32;
    let work_bottom = work.y + work.height as i32;
    let (x, bar_y) = match anchor {
        Some((x, y)) => (x, y),
        None => {
            let x = (right - w as i32).clamp(work.x, work_right - w as i32);
            let y = if work_bottom - bottom >= bar + gap {
                bottom + gap
            } else if region.top - work.y >= bar + gap {
                region.top - gap - bar
            } else {
                bottom - bar - gap
            };
            (x, y)
        }
    };
    let x = x.clamp(work.x, work_right - w as i32);
    let bar_y = bar_y.clamp(work.y, work_bottom - bar);
    let above = bar_y + bar + extra > work_bottom && bar_y - extra >= work.y;
    let y = if above { bar_y - extra } else { bar_y };
    (
        Placement {
            x,
            y,
            width: w,
            height: (bar + extra) as u32,
        },
        above,
    )
}

fn native_setup(h: HWND, placement: Placement) -> Result<()> {
    unsafe {
        // 工具栏、倒计时都不进入成片；系统不支持排除时拒绝录制，避免把控件录进去。
        // 远程画面兼容时保持可见：工具栏摆在选区外，只有选区外放不下时才会录进去。
        crate::remote_display::exclude_from_capture(h)
            .map_err(|e| format!("无法排除录屏工具栏：{e}"))?;
        SetWindowLongPtrW(h, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
        SetWindowLongPtrW(
            h,
            GWL_EXSTYLE,
            GetWindowLongPtrW(h, GWL_EXSTYLE) | WS_EX_TOOLWINDOW.0 as isize,
        );
        SetWindowPos(
            h,
            Some(HWND_TOPMOST),
            placement.x,
            placement.y,
            placement.width as i32,
            placement.height as i32,
            SWP_SHOWWINDOW | SWP_FRAMECHANGED | SWP_NOACTIVATE,
        )?;
    }
    Ok(())
}

/// 去掉 canonicalize 的 verbatim 前缀：`\\?\UNC\srv\share` 还原为 `\\srv\share`，`\\?\C:\` 还原为 `C:\`。
fn display_path_text(text: &str) -> String {
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(text).to_string()
    }
}

/// 以 CF_HDROP 复制文件，聊天软件和资源管理器可直接粘贴。
pub(crate) fn copy_file_to_clipboard(owner: HWND, path: &Path) -> Result<()> {
    use windows::Win32::{
        Foundation::{GlobalFree, HANDLE},
        System::{
            DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
            Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
            Ole::CF_HDROP,
        },
        UI::Shell::DROPFILES,
    };
    let path = std::fs::canonicalize(path)?;
    let text = display_path_text(&path.to_string_lossy());
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.extend([0, 0]);
    let header = size_of::<DROPFILES>();
    let bytes = header + wide.len() * 2;
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
        let data = GlobalLock(memory).cast::<u8>();
        if data.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err(tr!("无法分配剪贴板内存", "Couldn't allocate clipboard memory").into());
        }
        let files = DROPFILES {
            pFiles: header as u32,
            fWide: true.into(),
            ..Default::default()
        };
        std::ptr::write_unaligned(data.cast::<DROPFILES>(), files);
        std::ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), data.add(header), wide.len() * 2);
        let _ = GlobalUnlock(memory);
        let mut opened = false;
        for _ in 0..8 {
            if OpenClipboard(Some(owner)).is_ok() {
                opened = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
        if !opened {
            let _ = GlobalFree(Some(memory));
            return Err(tr!(
                "剪贴板正被其他程序占用，请重试",
                "Clipboard is in use by another app; try again"
            )
            .into());
        }
        let result = EmptyClipboard().and_then(|()| {
            SetClipboardData(u32::from(CF_HDROP.0), Some(HANDLE(memory.0))).map(|_| ())
        });
        let _ = CloseClipboard();
        if let Err(error) = result {
            let _ = GlobalFree(Some(memory));
            return Err(error.into());
        }
    }
    Ok(())
}

/// 录屏界面冒烟：宿主不激活 → 开始录制 → 停止 → 框内预览出帧 → 重录删除 → 关闭。
/// 设置与录屏历史隔离到 target/qa，结果写入 target/qa/recording-ui-smoke.json。
pub(crate) fn smoke() -> Result<()> {
    use gpui::Application;
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    std::env::set_var("RUSTSHOT_DATA_DIR", "target/qa/recording-ui/settings");
    let outcome = Arc::new(Mutex::new(None::<std::result::Result<String, String>>));
    let shared = outcome.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            crate::theme::install(cx);
            let work = monitor_work((0, 0));
            let region = ScreenRect {
                left: work.x + 80,
                top: work.y + 80,
                width: 361,
                height: 201,
            };
            let handle = match RecordingView::open_with(region, false, cx) {
                Ok(handle) => handle,
                Err(error) => {
                    *shared.lock().unwrap() = Some(Err(error.to_string()));
                    cx.quit();
                    return;
                }
            };
            let shared = shared.clone();
            cx.spawn(async move |cx| {
                let result = smoke_steps(handle, region, work, cx).await;
                *shared.lock().unwrap() = Some(result);
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });
    let result = outcome.lock().unwrap().take().ok_or("录屏界面冒烟未完成")?;
    let report = result.map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    std::fs::create_dir_all("target/qa")?;
    std::fs::write("target/qa/recording-ui-smoke.json", &report)?;
    println!("{report}");
    Ok(())
}

async fn smoke_steps(
    handle: WindowHandle<RecordingView>,
    region: ScreenRect,
    work: Placement,
    cx: &mut gpui::AsyncApp,
) -> std::result::Result<String, String> {
    let executor = cx.background_executor().clone();
    let wait = |ms: u64| executor.timer(Duration::from_millis(ms));
    wait(400).await;
    // 工具栏按内容定宽；说明气泡和消息条是独立浮层，不改变工具栏窗口。
    let idle_width = handle
        .update(cx, |v, w, _| {
            let expected = ((v.width() * crate::input_overlay::region_scale(region)).round()
                as u32)
                .min(work.width);
            if v.placement.width != expected {
                return Err(format!(
                    "待录工具栏宽度 {}，期望 {expected}",
                    v.placement.width
                ));
            }
            let height = v.placement.height;
            v.hover_changed("rec-fps", tr!("帧率", "Frame rate").into(), true, w);
            v.show_tip(w);
            let tip = v.tip.as_ref().and_then(tip::Bubble::size);
            v.hover_changed("rec-fps", SharedString::default(), false, w);
            if tip.is_none() {
                return Err("悬停后没有显示说明气泡".into());
            }
            if v.tip.as_ref().and_then(tip::Bubble::size).is_some() {
                return Err("离开按钮后说明气泡没有收起".into());
            }
            v.warn(tr!("冒烟消息", "Smoke notice"));
            Ok((v.placement.width, height))
        })
        .map_err(|e| e.to_string())??;
    wait(200).await;
    handle
        .update(cx, |v, _, _| {
            if v.toast.as_ref().and_then(tip::Bubble::size).is_none() {
                return Err("警告消息没有显示消息条".to_string());
            }
            if v.placement.height != idle_width.1 {
                return Err(format!(
                    "消息条改变了工具栏高度 {} → {}",
                    idle_width.1, v.placement.height
                ));
            }
            v.notice = None;
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    wait(200).await;
    handle
        .update(cx, |v, _, cx| {
            if v.toast.as_ref().and_then(tip::Bubble::size).is_some() {
                return Err("消息清除后消息条没有收起".to_string());
            }
            if v.border.is_none() {
                return Err(format!("选区边框未创建：{}", v.notice_text()));
            }
            v.toggle_record(cx);
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    wait(1600).await;
    let frames = handle
        .update(cx, |v, _, _| {
            v.recording
                .as_ref()
                .map(|r| r.frames.load(Ordering::Relaxed))
                .ok_or_else(|| format!("开始录制后没有录制会话：{}", v.notice_text()))
        })
        .map_err(|e| e.to_string())??;
    if frames == 0 {
        return Err("录制 1.6 秒没有采集到帧".into());
    }
    handle
        .update(cx, |v, _, cx| v.toggle_record(cx))
        .map_err(|e| e.to_string())?;
    let mut preview_ready = false;
    for _ in 0..300 {
        wait(50).await;
        let ready = handle
            .update(cx, |v, _, cx| {
                v.stage == Stage::Preview
                    && v.preview
                        .and_then(|p| p.read(cx).ok().map(|p| p.has_frame()))
                        .unwrap_or(false)
            })
            .map_err(|e| e.to_string())?;
        if ready {
            preview_ready = true;
            break;
        }
    }
    if !preview_ready {
        let status = handle
            .update(cx, |v, _, _| format!("{:?} {}", v.stage, v.notice_text()))
            .unwrap_or_default();
        return Err(format!("停止后未进入框内预览：{status}"));
    }
    wait(600).await;
    let (path, width, bar_width, position, bytes) = handle
        .update(cx, |v, _, cx| {
            let (_, position, _, _, _) = v.player_state(cx);
            let path = v.output.clone().unwrap_or_default();
            let bytes = std::fs::metadata(&path).map_or(0, |m| m.len());
            (path, v.placement.width, v.width(), position, bytes)
        })
        .map_err(|e| e.to_string())?;
    if bytes < 1000 {
        return Err(format!("录屏文件无效：{}", path.display()));
    }
    if position.is_zero() {
        return Err("框内预览没有自动播放".into());
    }
    let expected =
        ((bar_width * crate::input_overlay::region_scale(region)).round() as u32).min(work.width);
    if width != expected {
        return Err(format!("预览工具栏宽度 {width}，期望 {expected}"));
    }
    handle
        .update(cx, |v, _, cx| v.rerecord(cx))
        .map_err(|e| e.to_string())?;
    wait(300).await;
    let idle = handle
        .update(cx, |v, _, _| v.stage == Stage::Idle && v.preview.is_none())
        .map_err(|e| e.to_string())?;
    if !idle || path.exists() {
        return Err("重新录制没有回到待录或未删除本次文件".into());
    }
    handle
        .update(cx, |v, w, cx| v.close(w, cx))
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "{{\"test\":\"recording_ui_smoke\",\"frames\":{frames},\"bytes\":{bytes},\"previewPositionMs\":{},\"idleToolbarWidth\":{},\"toolbarWidth\":{width},\"tipAndNotice\":true,\"ok\":true}}",
        position.as_millis(),
        idle_width.0
    ))
}

#[cfg(all(test, windows))]
#[path = "recording_preview_retry_smoke.rs"]
mod preview_retry_smoke;

#[cfg(all(test, windows))]
#[path = "recording_preview_lifecycle_smoke.rs"]
mod preview_lifecycle_smoke;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_rate_labels_trim_zeroes() {
        assert_eq!(trim_rate(1.), "1");
        assert_eq!(trim_rate(1.25), "1.25");
        assert_eq!(trim_rate(0.5), "0.5");
    }

    #[test]
    fn clipboard_path_strips_verbatim_prefix() {
        assert_eq!(display_path_text(r"\\?\C:\a\b.mp4"), r"C:\a\b.mp4");
        assert_eq!(
            display_path_text(r"\\?\UNC\server\share\f.mp4"),
            r"\\server\share\f.mp4"
        );
        assert_eq!(display_path_text(r"C:\a\b.mp4"), r"C:\a\b.mp4");
        assert_eq!(
            display_path_text(r"\\server\share\f.mp4"),
            r"\\server\share\f.mp4"
        );
    }
}
