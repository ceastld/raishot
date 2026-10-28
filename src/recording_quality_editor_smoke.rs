//! 显式自有窗口专项：真实面板输入与请求解析，不进入录制或媒体后端。
use super::super::{
    button_bounds, device_choices, output_state, volume_intent, BAR_HEIGHT, BUTTON_BOUNDS,
    IDLE_COLOR,
};
use super::*;
use crate::{
    audio_capture::MixLevels, gpui_capture::hwnd, gpui_smoke::Input, pin_model::Placement,
    recording::scale_plan::ResizeRequest, recording_encoder_policy::EncoderPolicy,
};
use futures::FutureExt;
use gpui::{
    size, Application, AsyncApp, Bounds, Render, WeakEntity, WindowBounds, WindowHandle,
    WindowOptions,
};
use serde_json::{json, Value};
use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::windows::fs::MetadataExt,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT, WPARAM},
    UI::{
        Input::KeyboardAndMouse::{VK_BACK, VK_END, VK_TAB},
        WindowsAndMessaging::{
            GetClientRect, IsWindow, PostMessageW, WM_CHAR, WM_KEYDOWN, WM_KEYUP,
        },
    },
};

const WAIT: Duration = Duration::from_secs(3);
const BUDGET: Duration = Duration::from_secs(20);
const TICK: Duration = Duration::from_millis(25);
const REPORT_LIMIT: usize = 128 * 1024;
const UNSAFE_ATTRIBUTES: u32 = 0x400 | 0x1000 | 0x40000 | 0x400000;

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    previous: [Option<OsString>; 2],
}
impl Fixture {
    fn new() -> Result<Self> {
        let parent = std::path::absolute("target/recording-quality-ui")?;
        fs::create_dir_all(&parent)?;
        let metadata = fs::symlink_metadata(&parent)?;
        ensure(
            metadata.is_dir() && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0,
            "专项目录不是安全本地目录",
        )?;
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        for index in 0..16 {
            let root = parent.join(format!("native-{}-{stamp}-{index}", std::process::id()));
            match fs::create_dir(&root) {
                Ok(()) => {
                    let data = root.join("data");
                    fs::create_dir(&data)?;
                    let previous = [
                        std::env::var_os("RUSTSHOT_DATA_DIR"),
                        std::env::var_os("RUSTSHOT_EXPERIMENTAL_MEDIA"),
                    ];
                    std::env::set_var("RUSTSHOT_DATA_DIR", &data);
                    std::env::set_var("RUSTSHOT_EXPERIMENTAL_MEDIA", "1");
                    return Ok(Self {
                        root,
                        data,
                        previous,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err("专项独占目录冲突过多".into())
    }
    fn isolated(&self) -> Result<()> {
        ensure(
            crate::preferences::data_dir() == self.data
                && crate::file_launch_command::experimental_media_enabled(),
            "专项配置隔离或实验门已变化",
        )
    }
    fn saved_options(&self) -> Result<Value> {
        self.isolated()?;
        // 生产 RecordingOptions::save 的实际路径；只读取本专项隔离目录。
        let path = crate::preferences::data_dir().join("recording.json");
        ensure(
            path.parent() == Some(self.data.as_path()),
            "录屏配置写出隔离目录",
        )?;
        let metadata = fs::symlink_metadata(&path)?;
        ensure(
            metadata.is_file()
                && metadata.file_attributes() & UNSAFE_ATTRIBUTES == 0
                && metadata.len() <= 64 * 1024,
            "专项配置不是有界普通文件",
        )?;
        let mut bytes = Vec::new();
        fs::File::open(&path)?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure(
            bytes.len() as u64 == metadata.len(),
            "专项配置读取期间长度变化",
        )?;
        let value: Value = serde_json::from_slice(&bytes)?;
        ensure(
            value.get("fps").and_then(Value::as_u64) == Some(60),
            "真实 FPS 回调未保存 60 FPS",
        )?;
        Ok(json!({"path":path,"fps":60,"bytes":bytes.len(),"owned_fresh_configuration":true}))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for (key, value) in ["RUSTSHOT_DATA_DIR", "RUSTSHOT_EXPERIMENTAL_MEDIA"]
            .into_iter()
            .zip(&self.previous)
        {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn region() -> ScreenRect {
    ScreenRect {
        left: 120,
        top: 120,
        width: 385,
        height: 257,
    }
}

fn memory_view(window: &Window, cx: &mut Context<RecordingView>) -> RecordingView {
    let options = RecordingOptions {
        fps: 30,
        ..Default::default()
    };
    RecordingView {
        region: region(),
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
        border: None,
        border_color: IDLE_COLOR,
        preview: None,
        preview_audio_warning: None,
        drawing: None,
        countdown: None,
        flyout: None,
        region_drag: None,
        focus: cx.focus_handle(),
        key_owner: None,
        picking_save: false,
        notice: None,
        flash: None,
        size_label: None,
        tip: None,
        toast: None,
        tip_warm_until: None,
        tip_suppressed: None,
        bar_width: 1000.,
        light: crate::theme::is_light(cx),
        annotation_open: false,
        scale: window.scale_factor(),
        placement: Placement {
            x: 0,
            y: 0,
            width: 1000,
            height: BAR_HEIGHT as u32,
        },
        flyout_above: false,
        anchor: None,
        seeking: false,
        closing: false,
        hover: None,
        warnings_shown: 0,
        placing_until: None,
        resize_handles_pending: false,
        trace_opened: None,
    }
}

struct Host;
impl Render for Host {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().bg(rgb(crate::theme::palette(cx).background))
    }
}
#[derive(Default)]
struct Owner {
    view: Cell<Option<WindowHandle<RecordingView>>>,
    weak: RefCell<Option<WeakEntity<RecordingView>>>,
    native: Cell<usize>,
    host: Cell<Option<WindowHandle<Host>>>,
    host_weak: RefCell<Option<WeakEntity<Host>>>,
    host_native: Cell<usize>,
}
fn gone<T: 'static>(weak: &RefCell<Option<WeakEntity<T>>>, native: usize) -> bool {
    weak.borrow()
        .as_ref()
        .is_some_and(|weak| weak.upgrade().is_none())
        && native != 0
        && !unsafe { IsWindow(Some(HWND(native as *mut _))) }.as_bool()
}
fn released(owner: &Owner) -> bool {
    gone(&owner.weak, owner.native.get()) && gone(&owner.host_weak, owner.host_native.get())
}

fn invariant(view: &RecordingView, fixture: &Fixture) -> Result<()> {
    fixture.isolated()?;
    ensure(
        view.stage == Stage::Idle
            && view.region == region()
            && view.options.preset == Preset::CompactMp4
            && !view.options.microphone
            && !view.options.system_audio
            && view.recording.is_none()
            && view.abandoned.is_none()
            && view.result.is_none()
            && view.output.is_none()
            && view.gif_job.is_none()
            && view.mix_job.is_none()
            && view.preview.is_none()
            && view.border.is_none()
            && view.key_owner.is_none()
            && view.drawing.is_none()
            && view.countdown.is_none()
            && view.audio_input_check.is_none()
            && !view.audio_input_stopping
            && !view.record_after_input_check
            && view.last_encoding_session.is_none()
            && view.published_copy.is_none()
            && !view.picking_save
            && !view.closing
            && !view.resize_handles_pending
            && view.tip.is_none()
            && view.toast.is_none(),
        "专项离开了无录制/设备/输出的内存 Idle 状态",
    )
}

async fn tick(cx: &mut AsyncApp) {
    cx.background_executor().timer(TICK).await;
}
struct Ui<'a> {
    handle: WindowHandle<RecordingView>,
    fixture: &'a Fixture,
    owner: &'a Owner,
    report: &'a RefCell<Value>,
    started: Instant,
}
impl Ui<'_> {
    async fn wait(
        &self,
        cx: &mut AsyncApp,
        mut predicate: impl FnMut(
            &mut RecordingView,
            &mut Window,
            &mut Context<RecordingView>,
        ) -> Result<bool>,
    ) -> Result<()> {
        let waiting = Instant::now();
        while waiting.elapsed() < WAIT && self.started.elapsed() < BUDGET {
            if self.handle.update(cx, |view, window, cx| {
                invariant(view, self.fixture)?;
                predicate(view, window, cx)
            })?? {
                return Ok(());
            }
            tick(cx).await;
        }
        Err("面板条件未在调用间预算内满足".into())
    }
    async fn click(
        &self,
        flyout: Option<Flyout>,
        key: &'static str,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        self.handle.update(cx, |view, _, cx| -> Result<()> {
            invariant(view, self.fixture)?;
            ensure(view.flyout == flyout, "点击前面板阶段不符")?;
            BUTTON_BOUNDS.with(|map| map.borrow_mut().clear());
            cx.notify();
            Ok(())
        })??;
        self.wait(cx, |view, window, _| {
            Ok(view.flyout == flyout && input_point(window, self.owner, key).is_ok())
        })
        .await?;
        let (input, point) = self.handle.update(cx, |view, window, _| -> Result<_> {
            invariant(view, self.fixture)?;
            ensure(view.flyout == flyout, "实测坐标所属面板已变化")?;
            input_point(window, self.owner, key)
        })??;
        let mut value = self.report.borrow_mut();
        value["stage"] = key.into();
        let clicks = value["clicks"].as_array_mut().ok_or("点击证据格式错误")?;
        ensure(clicks.len() < 32, "点击证据超过固定上限")?;
        clicks.push(json!({"key":key,"dip":point,"scale":input.scale,"hwnd":input.handle}));
        drop(value);
        input.click(point);
        Ok(())
    }
    async fn advanced(&self, cx: &mut AsyncApp) -> Result<()> {
        for (before, key, after) in [
            (None, "rec-preset", Flyout::Preset),
            (Some(Flyout::Preset), "rec-quality-options", Flyout::Quality),
            (
                Some(Flyout::Quality),
                "rec-quality-advanced",
                Flyout::AdvancedQuality,
            ),
        ] {
            self.click(before, key, cx).await?;
            self.wait(cx, |view, _, _| Ok(view.flyout == Some(after)))
                .await?;
        }
        Ok(())
    }
    async fn width(&self, cx: &mut AsyncApp) -> Result<()> {
        self.click(Some(Flyout::AdvancedQuality), "rec-quality-size-width", cx)
            .await?;
        self.wait(cx, |view, window, cx| {
            Ok(window.is_window_active()
                && view
                    .quality_editor
                    .resize_width
                    .read(cx)
                    .focus_handle
                    .is_focused(window))
        })
        .await
    }
}

fn input_point(window: &Window, owner: &Owner, key: &'static str) -> Result<(Input, (f32, f32))> {
    let bounds = button_bounds(key).ok_or("本轮控件尚未真实布局")?;
    let point = (
        f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.,
        f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.,
    );
    let handle = hwnd(window);
    let scale = window.scale_factor();
    let mut client = RECT::default();
    unsafe { GetClientRect(handle, &mut client) }?;
    ensure(
        handle.0 as usize == owner.native.get()
            && unsafe { IsWindow(Some(handle)) }.as_bool()
            && f32::from(bounds.size.width) > 0.
            && f32::from(bounds.size.height) > 0.
            && scale.is_finite()
            && scale > 0.
            && point.0.is_finite()
            && point.1.is_finite()
            && point.0 > 0.
            && point.1 > 0.
            && point.0 * scale < client.right as f32
            && point.1 * scale < client.bottom as f32,
        "实测控件不在专项自有 HWND 客户区",
    )?;
    Ok((
        Input {
            handle: owner.native.get(),
            scale,
        },
        point,
    ))
}
fn post(owner: &Owner, message: u32, key: usize, flags: isize) -> Result<()> {
    let handle = HWND(owner.native.get() as *mut _);
    ensure(
        owner.native.get() != 0 && unsafe { IsWindow(Some(handle)) }.as_bool(),
        "字符目标不是存活的专项窗口",
    )?;
    unsafe { PostMessageW(Some(handle), message, WPARAM(key), LPARAM(flags)) }?;
    Ok(())
}
fn text(owner: &Owner, text: &str) -> Result<()> {
    ensure(
        text.len() <= 8 && text.is_ascii(),
        "专项只允许有界 ASCII 字符",
    )?;
    for unit in text.encode_utf16() {
        post(owner, WM_CHAR, usize::from(unit), 0)?;
    }
    Ok(())
}
fn key(owner: &Owner, value: u16) -> Result<()> {
    post(owner, WM_KEYDOWN, usize::from(value), 0)?;
    post(owner, WM_KEYUP, usize::from(value), (3_u32 << 30) as isize)
}

fn request(view: &mut RecordingView, cx: &App) -> Result<Option<ResolvedDraftRequest>> {
    let editor = &view.quality_editor;
    let width = editor.resize_width.read(cx).text();
    let height = editor.resize_height.read(cx).text();
    ensure(
        (matches!(width, "192" | "193") && height == "128")
            || (width.is_empty() && height.is_empty()),
        "输入不再是专项固定样本，未记录其内容",
    )?;
    ensure(
        editor.fields()[2..]
            .iter()
            .all(|field| field.read(cx).text().is_empty()),
        "其它字段出现非样本输入，未记录其内容",
    )?;
    let options = view.options.clone();
    view.quality_editor
        .request_with_resize(view.region, &options, view.quality_profile, true, cx)
}
fn valid_request(view: &mut RecordingView, fps: u32, cx: &App) -> Result<Value> {
    let request = request(view, cx)?.ok_or("实际请求没有启用")?;
    let source = RecordingSizeLayout::new(view.region)?;
    ensure(
        (
            source.source_width,
            source.source_height,
            source.encoded_width,
            source.encoded_height,
        ) == (385, 257, 384, 256)
            && request.resize
                == Some(ResizeRequest {
                    target_width: 192,
                    target_height: 128,
                })
            && (
                request.quality.plan.width,
                request.quality.plan.height,
                request.quality.plan.fps,
            ) == (192, 128, fps)
            && view.quality_editor.draft.resize.width_text == "192"
            && view.quality_editor.draft.resize.height_text == "128",
        "真实字段没有解析为精确目标尺寸/FPS",
    )?;
    Ok(
        json!({"source":source,"request":request.quality,"resize":request.resize,"raw":["192","128"]}),
    )
}

async fn exercise(
    fixture: &Fixture,
    owner: &Owner,
    report: &RefCell<Value>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let started = Instant::now();
    let handle = cx.update(|cx| -> Result<_> {
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1100.), px(600.)),
                    cx,
                ))),
                focus: true,
                ..Default::default()
            },
            |window, cx| {
                owner.native.set(hwnd(window).0 as usize);
                window.set_window_title(&format!(
                    "{} · {}",
                    crate::brand::NAME,
                    tr!("画质请求检查", "Quality request check")
                ));
                cx.new(|cx| {
                    *owner.weak.borrow_mut() = Some(cx.entity().downgrade());
                    memory_view(window, cx)
                })
            },
        )?;
        owner.view.set(Some(handle));
        handle.update(cx, |view, window, cx| -> Result<()> {
            crate::gpui_smoke::isolate_input(hwnd(window))?;
            invariant(view, fixture)?;
            window.focus(&view.focus);
            cx.notify();
            Ok(())
        })??;
        Ok(handle)
    })??;
    let ui = Ui {
        handle,
        fixture,
        owner,
        report,
        started,
    };
    ui.wait(cx, |_, window, _| Ok(window.is_window_active()))
        .await?;
    report.borrow_mut()["owned_window_active_observed"] = true.into();
    ui.advanced(cx).await?;
    ui.click(Some(Flyout::AdvancedQuality), "rec-quality-enabled", cx)
        .await?;
    ui.wait(cx, |view, _, _| Ok(view.quality_editor.draft.enabled))
        .await?;
    ui.click(Some(Flyout::AdvancedQuality), "rec-quality-size-set", cx)
        .await?;
    ui.wait(cx, |view, _, _| {
        Ok(view.quality_editor.draft.resize.mode == NumberMode::Set
            && view.quality_editor.enabled_mask.get() == 3)
    })
    .await?;
    ui.width(cx).await?;
    text(owner, "192")?;
    ui.wait(cx, |view, _, cx| {
        Ok(view.quality_editor.resize_width.read(cx).text() == "192")
    })
    .await?;
    key(owner, VK_TAB.0)?;
    ui.wait(cx, |view, window, cx| {
        Ok(window.is_window_active()
            && view
                .quality_editor
                .resize_height
                .read(cx)
                .focus_handle
                .is_focused(window))
    })
    .await?;
    report.borrow_mut()["tab_height_focus_observed"] = true.into();
    text(owner, "128")?;
    ui.wait(cx, |view, _, cx| {
        Ok(view.quality_editor.resize_height.read(cx).text() == "128")
    })
    .await?;
    report.borrow_mut()["request_30"] =
        handle.update(cx, |view, _, cx| valid_request(view, 30, cx))??;

    ui.width(cx).await?;
    key(owner, VK_END.0)?;
    key(owner, VK_BACK.0)?;
    text(owner, "3")?;
    ui.wait(cx, |view, _, cx| {
        Ok(view.quality_editor.resize_width.read(cx).text() == "193")
    })
    .await?;
    let invalid = handle.update(cx,|view,_,cx| -> Result<_> {
        let error = request(view,cx).err().ok_or("奇数目标宽度未被拒绝")?;
        ensure(view.quality_editor.error.is_some() && view.quality_editor.draft.resize.width_text == "193" && view.quality_editor.draft.resize.height_text == "128", "错误请求未保留真实输入")?;
        Ok(json!({"error":error.to_string(),"raw":["193","128"],"enabled_mask":view.quality_editor.enabled_mask.get()}))
    })??;
    report.borrow_mut()["invalid_request"] = invalid;
    ui.click(Some(Flyout::AdvancedQuality), "rec-fps", cx)
        .await?;
    ui.wait(cx, |view, _, _| {
        Ok(view.flyout == Some(Flyout::Fps) && view.quality_editor.error.is_some())
    })
    .await?;
    ui.click(Some(Flyout::Fps), "fps-60", cx).await?;
    ui.wait(cx, |view, _, cx| {
        Ok(view.options.fps == 60
            && view.flyout.is_none()
            && view.quality_editor.error.is_none()
            && view.quality_editor.draft.resize.mode == NumberMode::Set
            && view.quality_editor.resize_width.read(cx).text() == "193"
            && view.quality_editor.resize_height.read(cx).text() == "128"
            && view.quality_editor.last_context
                == Some((
                    view.region,
                    60,
                    view.options.preset,
                    view.quality_profile,
                    true,
                )))
    })
    .await?;
    report.borrow_mut()["context_error_cleared_without_rewriting_raw"] = true.into();
    report.borrow_mut()["saved_options"] = fixture.saved_options()?;
    ui.advanced(cx).await?;
    ui.width(cx).await?;
    key(owner, VK_END.0)?;
    key(owner, VK_BACK.0)?;
    text(owner, "2")?;
    ui.wait(cx, |view, _, cx| {
        Ok(view.quality_editor.resize_width.read(cx).text() == "192")
    })
    .await?;
    report.borrow_mut()["request_60"] =
        handle.update(cx, |view, _, cx| valid_request(view, 60, cx))??;
    ui.click(
        Some(Flyout::AdvancedQuality),
        "rec-quality-size-inherit",
        cx,
    )
    .await?;
    ui.wait(cx, |view, _, _| {
        Ok(view.quality_editor.draft.resize.mode == NumberMode::Inherit
            && view.quality_editor.enabled_mask.get() == 0)
    })
    .await?;
    report.borrow_mut()["inherit"] = handle.update(cx,|view,_,cx| -> Result<_> {
        let request = request(view,cx)?.ok_or("继承尺寸时高级请求意外停用")?;
        ensure(request.resize.is_none() && (request.quality.plan.width,request.quality.plan.height,request.quality.plan.fps) == (384,256,60)
            && view.quality_editor.draft.resize.width_text == "192" && view.quality_editor.draft.resize.height_text == "128", "继承没有保留原文或回到源尺寸")?;
        Ok(json!({"request":request.quality,"resize":request.resize,"raw":["192","128"],"enabled_mask":view.quality_editor.enabled_mask.get()}))
    })??;
    ui.click(Some(Flyout::AdvancedQuality), "rec-quality-reset", cx)
        .await?;
    ui.wait(cx, |view, window, cx| {
        Ok(view.quality_editor.draft == QualityDraft::default()
            && view
                .quality_editor
                .fields()
                .iter()
                .all(|field| field.read(cx).text().is_empty())
            && view.quality_editor.enabled_mask.get() == 0
            && !view.quality_editor.has_focus(window, cx))
    })
    .await?;
    handle.update(cx, |view, _, cx| -> Result<()> {
        ensure(request(view, cx)?.is_none(), "Reset 后仍产生高级请求")
    })??;
    report.borrow_mut()["reset_disabled_inherited_all_seven_fields_empty"] = true.into();
    ui.click(Some(Flyout::AdvancedQuality), "rec-quality-close", cx)
        .await?;
    ui.wait(cx, |view, window, _| {
        Ok(view.flyout.is_none() && view.focus.is_focused(window))
    })
    .await?;
    report.borrow_mut()["close_returned_focus_to_root"] = true.into();
    report.borrow_mut()["stage"] = "completed".into();
    Ok(())
}

async fn cleanup(owner: &Owner, cx: &mut AsyncApp) -> Result<()> {
    let mut errors = Vec::new();
    if let Some(handle) = owner.view.get() {
        if let Err(error) = handle.update(cx, |_, window, _| window.remove_window()) {
            errors.push(error.to_string());
        }
    }
    if let Some(handle) = owner.host.get() {
        if let Err(error) = handle.update(cx, |_, window, _| window.remove_window()) {
            errors.push(error.to_string());
        }
    }
    let start = Instant::now();
    while start.elapsed() < WAIT {
        if released(owner) {
            return ensure(
                errors.is_empty(),
                &format!("自有窗口关闭返回错误：{}", errors.join("；")),
            );
        }
        tick(cx).await;
    }
    Err(format!("专项自有窗口/实体释放未证实：{}", errors.join("；")).into())
}
fn write_report(root: &Path, report: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(report)?;
    ensure(bytes.len() <= REPORT_LIMIT, "专项报告超过 128 KiB")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join("report.json"))?;
    file.write_all(&bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}
fn run() -> Result<()> {
    let fixture = Rc::new(Fixture::new()?);
    let owner = Rc::new(Owner::default());
    let report = Rc::new(RefCell::new(
        json!({"ok":false,"stage":"created","clicks":[],"report_limit_bytes":REPORT_LIMIT,
        "requires_owned_window_focus":true,
        "scope":["Dedicated exact GPUI process; fresh data directory and experimental gate precede theme/Preferences/Application initialization and remain until exit. System theme; the owned window is normally created with focus:true and its real active state is required for native text/Tab. No SetForegroundWindow, WM_ACTIVATE injection or state bypass; no claim of shielding physical keyboard/IME input or restoring another app's foreground window.",
        "Locally constructed Idle RecordingView uses the real production render/callbacks and QualityEditor request parser, without open_with/options load/native border/KeyOwner/poll or a recording/media/device worker. The real FPS callback saves only the fresh owned recording.json.",
        "Request DTOs are observations of the production parser, not a Record click, backend dispatch, real resampling, encoder result, IME composition, clipboard, dialog or physical user input. Only exact fixture strings may be reported; unexpected text is not logged. Budgets are cooperative and cannot interrupt blocking native/I/O calls."]}),
    ));
    let outcome = Rc::new(RefCell::new(Some("专项未完成".to_owned())));
    let (app_fixture, app_owner, app_report, app_outcome) = (
        fixture.clone(),
        owner.clone(),
        report.clone(),
        outcome.clone(),
    );
    let application = std::panic::catch_unwind(AssertUnwindSafe(|| {
        Application::new()
            .with_assets(crate::toolbar_icons::IconAssets)
            .run(move |cx| {
                crate::theme::set_mode(crate::theme::ThemeMode::System, cx);
                match cx.open_window(
                    WindowOptions {
                        show: false,
                        focus: false,
                        ..Default::default()
                    },
                    |window, cx| {
                        app_owner.host_native.set(hwnd(window).0 as usize);
                        cx.new(|cx| {
                            *app_owner.host_weak.borrow_mut() = Some(cx.entity().downgrade());
                            Host
                        })
                    },
                ) {
                    Ok(handle) => app_owner.host.set(Some(handle)),
                    Err(error) => {
                        *app_outcome.borrow_mut() = Some(error.to_string());
                        cx.quit();
                        return;
                    }
                }
                cx.spawn(async move |cx| {
                    let body =
                        AssertUnwindSafe(exercise(&app_fixture, &app_owner, &app_report, cx))
                            .catch_unwind()
                            .await;
                    let body_error = match body {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("面板交互捕获 panic".into()),
                    };
                    let closed = AssertUnwindSafe(cleanup(&app_owner, cx))
                        .catch_unwind()
                        .await;
                    let close_error = match closed {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("窗口清理捕获 panic".into()),
                    };
                    if let Some(error) = &body_error {
                        app_report.borrow_mut()["interaction_error"] =
                            error.chars().take(2048).collect::<String>().into();
                    }
                    if let Some(error) = &close_error {
                        app_report.borrow_mut()["cleanup_error"] =
                            error.chars().take(2048).collect::<String>().into();
                    }
                    *app_outcome.borrow_mut() = body_error.or(close_error);
                    let _ = cx.update(|cx| {
                        if let Some(handle) = app_owner.view.get() {
                            let _ = handle.update(cx, |_, window, _| window.remove_window());
                        }
                        if let Some(handle) = app_owner.host.get() {
                            let _ = handle.update(cx, |_, window, _| window.remove_window());
                        }
                        cx.quit();
                    });
                })
                .detach();
            })
    }));
    let resources = released(&owner);
    let ok = application.is_ok() && outcome.borrow().is_none() && resources;
    report.borrow_mut()["resources_released"] = resources.into();
    report.borrow_mut()["application_panicked"] = application.is_err().into();
    report.borrow_mut()["run_error"] = outcome
        .borrow()
        .as_ref()
        .map(|value| value.chars().take(2048).collect::<String>())
        .into();
    report.borrow_mut()["ok"] = ok.into();
    write_report(&fixture.root, &report.borrow())?;
    let _ = writeln!(
        std::io::stderr().lock(),
        "画质面板专项报告：{}",
        fixture.root.join("report.json").display()
    );
    assert!(ok, "画质面板专项失败；报告：{}", fixture.root.display());
    Ok(())
}

#[test]
#[ignore = "独立 exact GPUI 面板输入专项；不启动录制/媒体/设备，不使用用户配置或物理输入"]
fn native_recording_quality_resize_panel_smoke() -> Result<()> {
    run()
}
