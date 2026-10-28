//! 保存专项只操作合成图、自有窗口和 target/qa/save 隔离目录。
use crate::{
    annotation::Tool,
    canvas_source::CanvasSource,
    capture::Image,
    capture_view::CaptureView,
    export,
    fast_capture::{FrozenDesktop, FrozenScreen},
    geometry::{Rect, ScreenRect},
    gpui_capture::hwnd,
    gpui_smoke::{self, Input},
    pin_model::PinState,
    pin_view::PinView,
    preferences::Preferences,
    save_options::{PngCompression, SaveConfig, SaveEncoding, SaveFormat},
    save_panel::{SaveOutcome, SavePanel},
    save_store::SaveStore,
    Result,
};
use gpui::{
    div, prelude::*, px, size, App, AppContext, Application, AsyncApp, Bounds, Context, Keystroke,
    Render, RenderImage, TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};

const TIMEOUT: Duration = Duration::from_secs(75);
const WIDTH: u32 = 128;
const HEIGHT: u32 = 96;

fn ensure(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.to_string().into())
    }
}
fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}
struct Qa {
    root: PathBuf,
    data: PathBuf,
    file: File,
    started: Instant,
}
impl Qa {
    fn new() -> Result<Self> {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let root = std::env::current_dir()?.join(format!(
            "target/qa/save/session-{}-{stamp}",
            std::process::id()
        ));
        let data = root.join("settings");
        std::fs::create_dir_all(&data)?;
        std::env::set_var("RUSTSHOT_DATA_DIR", &data);
        std::env::set_var("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1");
        let file = File::create(root.join("ui.jsonl"))?;
        Ok(Self {
            root,
            data,
            file,
            started: Instant::now(),
        })
    }
    fn emit(&mut self, name: &str, detail: &str, ok: bool) -> Result<()> {
        let line = format!("{{\"suite\":\"save\",\"test\":\"{name}\",\"pid\":{},\"elapsed_ms\":{:.1},\"detail\":\"{}\",\"ok\":{ok}}}", std::process::id(), self.started.elapsed().as_secs_f64() * 1000., escape(detail));
        println!("{line}");
        writeln!(self.file, "{line}")?;
        self.file.flush()?;
        Ok(())
    }
    fn pass(&mut self, name: &str) -> Result<()> {
        ensure(self.started.elapsed() < TIMEOUT, "保存专项超时")?;
        self.emit(name, "仅合成图；没有读取桌面或剪贴板", true)
    }
}

fn fixture() -> Arc<Image> {
    let mut bgra = Vec::with_capacity(WIDTH as usize * HEIGHT as usize * 4);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let alpha = [0, 64, 128, 255][(x / 32) as usize];
            // 上方为平色 alpha 样本，下方细节用于独立检验 JPEG 量化变化。
            let color = if y < 32 {
                [30, 80, 190]
            } else {
                [
                    (x * 17 + y * 11) as u8,
                    (x * 7 + y * 19) as u8,
                    (x * 29 + y * 3) as u8,
                ]
            };
            bgra.extend_from_slice(&[color[0], color[1], color[2], alpha]);
        }
    }
    Arc::new(Image {
        width: WIDTH,
        height: HEIGHT,
        bgra,
        readback_ms: 0.,
    })
}
fn check_png(bytes: &[u8], image: &Image) -> Result<()> {
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)?.into_rgba8();
    ensure(
        decoded.dimensions() == (image.width, image.height),
        "PNG 尺寸变化",
    )?;
    for (actual, expected) in decoded
        .as_raw()
        .chunks_exact(4)
        .zip(image.bgra.chunks_exact(4))
    {
        ensure(
            actual == [expected[2], expected[1], expected[0], expected[3]],
            "PNG 未保留直色与透明 alpha",
        )?;
    }
    Ok(())
}
fn check_bmp(bytes: &[u8], image: &Image) -> Result<()> {
    let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Bmp)?.into_rgba8();
    ensure(
        decoded.dimensions() == (image.width, image.height),
        "BMP 尺寸变化",
    )?;
    for (actual, source) in decoded
        .as_raw()
        .chunks_exact(4)
        .zip(image.bgra.chunks_exact(4))
    {
        let a = u32::from(source[3]);
        let white = |color: u8| ((u32::from(color) * a + 255 * (255 - a) + 127) / 255) as u8;
        ensure(
            actual == [white(source[2]), white(source[1]), white(source[0]), 255],
            "BMP 透明内容没有合成为白底",
        )?;
    }
    Ok(())
}
/// 从 JPEG DQT 标记读取量化强度，不依赖生产质量映射或只比较文件长度。
fn jpeg_quantization(bytes: &[u8]) -> Result<u64> {
    ensure(bytes.starts_with(&[0xff, 0xd8]), "JPEG 缺少 SOI")?;
    let (mut offset, mut total, mut tables) = (2, 0_u64, 0);
    while offset + 4 <= bytes.len() {
        ensure(bytes[offset] == 0xff, "JPEG 标记边界无效")?;
        let marker = bytes[offset + 1];
        if marker == 0xda || marker == 0xd9 {
            break;
        }
        let length = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
        ensure(
            length >= 2 && offset + 2 + length <= bytes.len(),
            "JPEG 标记长度无效",
        )?;
        if marker == 0xdb {
            let mut data = &bytes[offset + 4..offset + 2 + length];
            while !data.is_empty() {
                let precision = data[0] >> 4;
                let width = if precision == 0 { 1 } else { 2 };
                ensure(precision <= 1 && data.len() > 64 * width, "JPEG DQT 无效")?;
                for value in data[1..1 + 64 * width].chunks_exact(width) {
                    total += if width == 1 {
                        u64::from(value[0])
                    } else {
                        u64::from(u16::from_be_bytes([value[0], value[1]]))
                    };
                }
                tables += 1;
                data = &data[1 + 64 * width..];
            }
        }
        offset += 2 + length;
    }
    ensure(tables > 0, "JPEG 缺少量化表")?;
    Ok(total)
}
fn encoding_cases(image: &Image, qa: &mut Qa) -> Result<()> {
    for compression in [
        PngCompression::Fast,
        PngCompression::Balanced,
        PngCompression::Small,
    ] {
        check_png(
            &export::encode_image(
                image,
                SaveEncoding {
                    png_compression: compression,
                    ..Default::default()
                },
            )?,
            image,
        )?;
    }
    check_bmp(
        &export::encode_image(
            image,
            SaveEncoding {
                format: SaveFormat::Bmp,
                ..Default::default()
            },
        )?,
        image,
    )?;
    qa.pass("save_encoding_png_alpha_bmp_white")?;
    let encode = |quality| {
        export::encode_image(
            image,
            SaveEncoding {
                format: SaveFormat::Jpeg,
                jpeg_quality: quality,
                ..Default::default()
            },
        )
    };
    let low = encode(35)?;
    let high = encode(95)?;
    ensure(
        jpeg_quantization(&low)? > jpeg_quantization(&high)?,
        "JPEG 质量没有改变实际量化表",
    )?;
    for bytes in [&low, &high] {
        let decoded =
            image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg)?.into_rgb8();
        ensure(decoded.dimensions() == (WIDTH, HEIGHT), "JPEG 尺寸变化")?;
        let white = decoded.get_pixel(8, 8).0;
        ensure(white.iter().all(|c| *c >= 248), "JPEG 全透明区域出现黑底")?;
        ensure(
            decoded.get_pixel(120, 8).0[0] > decoded.get_pixel(120, 8).0[2],
            "JPEG RGB/BGRA 色序反转",
        )?;
    }
    ensure(
        encode(0).is_err() && encode(101).is_err(),
        "非法 JPEG 质量被接受",
    )?;
    qa.pass("save_encoding_jpeg_real_quality")
}

fn disk_snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
        if !dir.exists() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                visit(root, &entry.path(), files)?;
            } else {
                files.insert(
                    entry.path().strip_prefix(root)?.to_path_buf(),
                    std::fs::read(entry.path())?,
                );
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}
fn initial_config(qa: &Qa) -> SaveConfig {
    SaveConfig {
        directory: qa.root.join("output"),
        filename_template: "QA_{width}x{height}".into(),
        ..Default::default()
    }
}
fn store_config(config: SaveConfig) -> Result<()> {
    let mut store = SaveStore::load();
    store.config = config;
    store.save()
}
fn quick_save_cases(image: &Image, qa: &mut Qa) -> Result<()> {
    let mut config = initial_config(qa);
    config.filename_template = "quick_{width}x{height}".into();
    std::fs::create_dir_all(&config.directory)?;
    let occupied = config.directory.join("quick_128x96.png");
    std::fs::write(&occupied, b"existing synthetic sentinel")?;
    store_config(config.clone())?;
    let history_before = crate::history::entries()?.len();
    let first = crate::save_workflow::quick_save(image, None, true)?;
    let second = crate::save_workflow::quick_save(image, None, true)?;
    ensure(
        first.path != second.path && first.path != occupied && second.path != occupied,
        "快速保存覆盖了同名文件",
    )?;
    ensure(
        std::fs::read(&occupied)? == b"existing synthetic sentinel",
        "快速保存破坏了原有同名文件",
    )?;
    check_png(&std::fs::read(first.path)?, image)?;
    ensure(
        crate::history::entries()?.len() == history_before + 2,
        "成功快速保存没有按次记录历史",
    )?;
    // 模板自身含受支持扩展名时，防撞后缀必须插到最终扩展名前。
    for format in [SaveFormat::Png, SaveFormat::Jpeg] {
        let mut with_extension = config.clone();
        with_extension.filename_template = "截图.png".into();
        with_extension.encoding.format = format;
        let occupied = with_extension
            .directory
            .join(format!("截图.{}", format.extension()));
        std::fs::write(&occupied, b"own extension collision fixture")?;
        let report = crate::save_workflow::quick_save_with(image, None, false, &with_extension)?;
        ensure(
            report.path.file_name().and_then(|name| name.to_str())
                == Some(format!("截图 (1).{}", format.extension()).as_str()),
            "模板含扩展名时防撞后缀位置错误",
        )?;
        ensure(
            std::fs::read(occupied)? == b"own extension collision fixture",
            "模板扩展名冲突覆盖了已有文件",
        )?;
    }
    let blocked = qa.root.join("not-a-directory");
    std::fs::write(&blocked, b"own fixture blocks directory creation")?;
    config.directory = blocked.join("child");
    store_config(config)?;
    let before = disk_snapshot(&qa.data)?;
    ensure(
        crate::save_workflow::quick_save(image, None, true).is_err(),
        "不可写目录快速保存未报错",
    )?;
    ensure(
        disk_snapshot(&qa.data)? == before,
        "快速保存失败仍改写了配置、目录记忆或历史",
    )?;
    store_config(initial_config(qa))?;
    qa.pass("save_quick_no_overwrite_failure_no_history")
}

async fn step(cx: &mut AsyncApp) {
    cx.background_executor()
        .timer(Duration::from_millis(120))
        .await;
}
struct Panel {
    window: WindowHandle<SavePanel>,
    input: Input,
    outcome: Rc<RefCell<Option<SaveOutcome>>>,
}
async fn open_panel(
    image: Arc<Image>,
    protected: Option<PathBuf>,
    cx: &mut AsyncApp,
) -> Result<Panel> {
    let outcome = Rc::new(RefCell::new(None));
    let callback = outcome.clone();
    let window = cx
        .update(|cx| {
            SavePanel::open(
                image,
                protected,
                "RustShot 保存验收（合成图）",
                move |result, _| {
                    *callback.borrow_mut() = Some(result);
                },
                cx,
            )
        })
        .map_err(|e| e.to_string())??;
    let input = window
        .update(cx, |_, w, _| {
            w.activate_window();
            Input {
                handle: hwnd(w).0 as usize,
                scale: w.scale_factor(),
            }
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    step(cx).await;
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(HWND(input.handle as *mut _), Some(&mut pid));
    }
    ensure(pid == std::process::id(), "保存专项输入目标不是本进程")?;
    Ok(Panel {
        window,
        input,
        outcome,
    })
}
impl Panel {
    async fn button(&self, id: &str, cx: &mut AsyncApp) -> Result<()> {
        for _ in 0..24 {
            let (position, height, width, body_bottom) = self
                .window
                .update(cx, |v, w, _| {
                    let positions = v.ui_positions.borrow();
                    let height = f32::from(w.viewport_size().height);
                    let footer_top = [
                        "save-remember",
                        "save-overwrite",
                        "save-keep-both",
                        "save-overwrite-cancel",
                    ]
                    .iter()
                    .filter_map(|id| positions.get(*id).map(|p| p.1 - 44.))
                    .min_by(f32::total_cmp)
                    .unwrap_or(height - 8.);
                    (
                        positions.get(id).copied(),
                        height,
                        f32::from(w.viewport_size().width),
                        footer_top,
                    )
                })
                .map_err(|e| e.to_string())?;
            let footer = matches!(
                id,
                "save-submit"
                    | "save-cancel"
                    | "save-remember"
                    | "save-overwrite"
                    | "save-keep-both"
                    | "save-overwrite-cancel"
            );
            let bottom = if footer { height - 2. } else { body_bottom };
            if let Some(p) = position {
                if p.0 > 2. && p.0 < width - 2. && p.1 > 2. && p.1 < bottom {
                    self.input.click(p);
                    step(cx).await;
                    return Ok(());
                }
                self.input.wheel(
                    (width / 2., body_bottom / 2.),
                    if p.1 <= 2. { 360 } else { -360 },
                )?;
            } else {
                self.input.wheel(
                    (width / 2., body_bottom / 2.),
                    if matches!(id, "save-name" | "save-directory") {
                        360
                    } else {
                        -360
                    },
                )?;
            }
            step(cx).await;
        }
        Err(format!("保存面板按钮不可见：{id}").into())
    }
    async fn text(&self, id: &str, text: &str, cx: &mut AsyncApp) -> Result<()> {
        self.button(id, cx).await?;
        // 仅向此 GPUI 窗口派发 Ctrl+A；不模拟全局按键或修改系统键盘状态。
        let select_all = Keystroke::parse("ctrl-a").map_err(|e| e.to_string())?;
        cx.update_window(self.window.into(), |_, w, cx| {
            w.dispatch_keystroke(select_all, cx);
        })
        .map_err(|e| e.to_string())?;
        for unit in text.encode_utf16() {
            unsafe {
                PostMessageW(
                    Some(HWND(self.input.handle as *mut _)),
                    WM_CHAR,
                    WPARAM(unit as usize),
                    LPARAM(0),
                )?;
            }
        }
        step(cx).await;
        let actual = self
            .window
            .update(cx, |v, _, cx| {
                match id {
                    "save-name" => v.filename.read(cx).text(),
                    "save-directory" => v.directory.read(cx).text(),
                    "save-template" => v.template.read(cx).text(),
                    "save-date-format" => v.date_format.read(cx).text(),
                    _ => "",
                }
                .to_string()
            })
            .map_err(|e| e.to_string())?;
        ensure(
            actual == text,
            &format!("原生文字输入没有替换目标字段：{id}"),
        )?;
        Ok(())
    }
    async fn encoded(&self, cx: &mut AsyncApp) -> Result<Arc<Vec<u8>>> {
        for _ in 0..60 {
            let (pending, encoded, status) = self
                .window
                .update(cx, |v, _, _| {
                    (v.encoding_pending, v.encoded.clone(), v.status.clone())
                })
                .map_err(|e| e.to_string())?;
            if !pending {
                return encoded.ok_or_else(|| format!("保存预编码失败：{status}").into());
            }
            step(cx).await;
        }
        Err("保存预编码未在限时内结束".into())
    }
    async fn completed(&self, cx: &mut AsyncApp) -> Result<SaveOutcome> {
        for _ in 0..60 {
            if let Some(outcome) = self.outcome.borrow_mut().take() {
                return Ok(outcome);
            }
            step(cx).await;
        }
        Err("保存面板未在限时内返回结果".into())
    }
    async fn failed_open(&self, expected_error: &str, cx: &mut AsyncApp) -> Result<()> {
        for _ in 0..60 {
            let (saving, status) = self
                .window
                .update(cx, |v, _, _| (v.saving, v.status.clone()))
                .map_err(|e| e.to_string())?;
            if !saving && status.contains(expected_error) {
                ensure(self.outcome.borrow().is_none(), "保存失败却结束了会话")?;
                ensure(
                    unsafe { IsWindowVisible(HWND(self.input.handle as *mut _)) }.as_bool(),
                    "保存失败关闭了自有窗口",
                )?;
                return Ok(());
            }
            step(cx).await;
        }
        Err("保存失败没有在限时内恢复会话".into())
    }
    async fn cancel(&self, cx: &mut AsyncApp) -> Result<()> {
        self.button("save-cancel", cx).await?;
        ensure(
            matches!(self.completed(cx).await?, SaveOutcome::Cancelled),
            "取消保存没有返回 Cancelled",
        )
    }
}

fn copy_image(image: &Image) -> Image {
    Image {
        width: image.width,
        height: image.height,
        bgra: image.bgra.clone(),
        readback_ms: 0.,
    }
}
async fn owner_panel(cx: &mut AsyncApp) -> Result<Panel> {
    for _ in 0..30 {
        let handle = cx
            .update(|cx| {
                cx.windows()
                    .into_iter()
                    .find_map(|window| window.downcast::<SavePanel>())
            })
            .map_err(|e| e.to_string())?;
        if let Some(window) = handle {
            let input = window
                .update(cx, |_, w, _| Input {
                    handle: hwnd(w).0 as usize,
                    scale: w.scale_factor(),
                })
                .map_err(|e| e.to_string())?;
            gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
            return Ok(Panel {
                window,
                input,
                outcome: Rc::new(RefCell::new(None)),
            });
        }
        step(cx).await;
    }
    Err("宿主快捷键未打开共享保存面板".into())
}
async fn capture_key(owner: WindowHandle<CaptureView>, key: &str, cx: &mut AsyncApp) -> Result<()> {
    let key = Keystroke::parse(key).map_err(|e| e.to_string())?;
    owner
        .update(cx, |v, w, _| {
            w.activate_window();
            w.focus(&v.focus);
        })
        .map_err(|e| e.to_string())?;
    cx.update_window(owner.into(), |_, w, cx| {
        w.dispatch_keystroke(key, cx);
    })
    .map_err(|e| e.to_string())?;
    step(cx).await;
    Ok(())
}
async fn owner_cases(image: Arc<Image>, qa: &mut Qa, cx: &mut AsyncApp) -> Result<()> {
    let original_path = qa.root.join("owner-original.png");
    export::save_encoded(
        &export::encode_image(&image, SaveEncoding::default())?,
        &original_path,
        false,
    )?;
    let original_bytes = std::fs::read(&original_path)?;
    let payload = copy_image(&image);
    let path = original_path.clone();
    let editor = cx
        .update(|cx| crate::image_editor::open(payload, Some(path), cx))
        .map_err(|e| e.to_string())??;
    step(cx).await;
    let before = disk_snapshot(&qa.data)?;
    capture_key(editor, "ctrl-s", cx).await?;
    let panel = owner_panel(cx).await?;
    ensure(
        editor
            .update(cx, |v, _, _| v.saving)
            .map_err(|e| e.to_string())?,
        "图片编辑另存未设置宿主 busy",
    )?;
    panel.button("save-cancel", cx).await?;
    capture_key(editor, "2", cx).await?;
    editor
        .update(cx, |v, _, _| -> Result<()> {
            ensure(
                !v.saving
                    && !v.processing
                    && v.image_canvas.is_some()
                    && v.editor.tool == Tool::Rectangle,
                "图片编辑取消另存后不能继续编辑",
            )
        })
        .map_err(|e| e.to_string())??;
    ensure(
        std::fs::read(&original_path)? == original_bytes && disk_snapshot(&qa.data)? == before,
        "图片编辑取消保存改写了原图或磁盘状态",
    )?;
    editor
        .update(cx, |_, w, _| w.remove_window())
        .map_err(|e| e.to_string())?;
    step(cx).await;
    qa.pass("save_owner_image_cancel_resumes_editing")?;

    let payload = copy_image(&image);
    let pin = cx
        .update(|cx| {
            PinView::open_state(
                payload,
                (220, 180),
                PinState {
                    zoom: 2.,
                    ..Default::default()
                },
                cx,
            )
        })
        .map_err(|e| e.to_string())??;
    step(cx).await;
    let input = pin
        .update(cx, |v, w, _| {
            w.activate_window();
            w.focus(&v.focus);
            Input {
                handle: hwnd(w).0 as usize,
                scale: w.scale_factor(),
            }
        })
        .map_err(|e| e.to_string())?;
    gpui_smoke::isolate_input(HWND(input.handle as *mut _))?;
    input.key(0x20);
    step(cx).await;
    pin.update(cx, |v, _, _| {
        ensure(
            v.chrome && v.canvas.is_some(),
            "贴图专项未建立标注画布和两级工具栏",
        )
    })
    .map_err(|e| e.to_string())??;
    let before = disk_snapshot(&qa.data)?;
    let key = Keystroke::parse("ctrl-s").map_err(|e| e.to_string())?;
    pin.update(cx, |v, w, _| {
        w.focus(&v.focus);
    })
    .map_err(|e| e.to_string())?;
    cx.update_window(pin.into(), |_, w, cx| {
        w.dispatch_keystroke(key, cx);
    })
    .map_err(|e| e.to_string())?;
    step(cx).await;
    let panel = owner_panel(cx).await?;
    pin.update(cx, |v, _, cx| -> Result<()> {
        ensure(
            v.saving
                && v.canvas
                    .as_ref()
                    .is_some_and(|canvas| canvas.read(cx).saving),
            "贴图另存未统一宿主和画布 busy",
        )
    })
    .map_err(|e| e.to_string())??;
    panel.button("save-cancel", cx).await?;
    input.key(0x32);
    step(cx).await;
    pin.update(cx, |v, _, cx| -> Result<()> {
        ensure(
            !v.saving
                && !v.processing
                && v.chrome
                && v.canvas.as_ref().is_some_and(|canvas| {
                    let canvas = canvas.read(cx);
                    !canvas.saving && canvas.editor.tool == Tool::Rectangle
                }),
            "贴图取消另存后宿主/标注画布仍 busy",
        )
    })
    .map_err(|e| e.to_string())??;
    ensure(
        disk_snapshot(&qa.data)? == before,
        "贴图取消保存仍写入配置或历史",
    )?;
    pin.update(cx, |v, w, cx| {
        v.close_toolbar(cx);
        w.remove_window();
    })
    .map_err(|e| e.to_string())?;
    step(cx).await;
    qa.pass("save_owner_pin_cancel_resumes_canvas")?;

    // 通过相同 CaptureView 事件路由测试截图快存；只将合成文件源替为合成桌面源。
    let mut opaque = copy_image(&image);
    for pixel in opaque.bgra.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    let opaque = Arc::new(opaque);
    let payload = copy_image(&opaque);
    let capture = cx
        .update(|cx| crate::image_editor::open(payload, None, cx))
        .map_err(|e| e.to_string())??;
    capture
        .update(cx, |view, window, cx| -> Result<()> {
            let bounds = ScreenRect {
                left: -1920,
                top: -72,
                width: WIDTH,
                height: HEIGHT,
            };
            let buffer = image::RgbaImage::from_raw(WIDTH, HEIGHT, opaque.bgra.clone())
                .ok_or("合成截图像素无效")?;
            let preview = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
            let (source, previews) = CanvasSource::frozen_desktop(FrozenDesktop {
                screens: vec![FrozenScreen {
                    rect: bounds,
                    preview,
                }],
                bounds,
                current: 0,
                capture_ms: 0.,
            });
            let focus = view.focus.clone();
            *view = CaptureView::new(
                source,
                previews,
                focus,
                Some(Rect::full(WIDTH, HEIGHT)),
                None,
                None,
                Vec::new(),
            );
            view.magnifier = false;
            view.include_cursor = false;
            view.guides = false;
            view.show_guides = false;
            view.finish_style = Default::default();
            window.focus(&view.focus);
            cx.notify();
            Ok(())
        })
        .map_err(|e| e.to_string())??;
    let mut config = initial_config(qa);
    config.filename_template = "owner_capture_{width}x{height}".into();
    config.directory = qa.root.join("owner-capture");
    store_config(config.clone())?;
    let history_before = crate::history::entries()?.len();
    capture_key(capture, "ctrl-shift-s", cx).await?;
    let mut closed = false;
    for _ in 0..60 {
        if capture.update(cx, |_, _, _| ()).is_err() {
            closed = true;
            break;
        }
        step(cx).await;
    }
    ensure(closed, "截图快存成功未关闭宿主会话")?;
    ensure(
        crate::history::entries()?.len() == history_before + 1,
        "截图快捷键快存没有恰好记录一次历史",
    )?;
    let saved = config.directory.join("owner_capture_128x96.png");
    check_png(&std::fs::read(saved)?, &opaque)?;
    qa.pass("save_owner_capture_quick_save_closes_records_once")
}

async fn verify(image: Arc<Image>, mut qa: Qa, cx: &mut AsyncApp) -> Result<()> {
    let before = disk_snapshot(&qa.data)?;
    let panel = open_panel(image.clone(), None, cx).await?;
    panel
        .text("save-template", "template_{width}x{height}", cx)
        .await?;
    let generated = panel
        .window
        .update(cx, |v, _, cx| v.filename.read(cx).text().to_string())
        .map_err(|e| e.to_string())?;
    ensure(
        generated == "template_128x96" || generated == "template_128x96.png",
        "模板没有生成真实图像尺寸文件名",
    )?;
    panel.text("save-name", "手改名称.png", cx).await?;
    panel.text("save-template", "changed_{width}", cx).await?;
    panel.button("save-format-jpeg", cx).await?;
    let high = panel.encoded(cx).await?;
    let prior_quality = panel
        .window
        .update(cx, |v, _, _| v.draft.encoding.jpeg_quality)
        .map_err(|e| e.to_string())?;
    panel.button("save-quality-minus", cx).await?;
    let low = panel.encoded(cx).await?;
    panel
        .window
        .update(cx, |v, _, cx| -> Result<()> {
            ensure(
                v.manual_filename && v.filename.read(cx).text().starts_with("手改名称"),
                "更改模板或格式覆盖了手改文件名",
            )?;
            ensure(
                v.draft.encoding.jpeg_quality < prior_quality,
                "JPEG 质量按钮未调整档位",
            )
        })
        .map_err(|e| e.to_string())??;
    ensure(
        jpeg_quantization(&low)? > jpeg_quantization(&high)?,
        "真实质量控件未改变预览编码量化表",
    )?;
    panel.cancel(cx).await?;
    ensure(
        disk_snapshot(&qa.data)? == before,
        "取消保存改写了配置、目录历史或截图历史",
    )?;
    qa.pass("save_panel_template_manual_cancel_no_writes")?;

    let panel = open_panel(image.clone(), None, cx).await?;
    panel
        .text("save-template", "remember_{width}x{height}", cx)
        .await?;
    panel
        .text(
            "save-directory",
            &qa.root.join("remembered").to_string_lossy(),
            cx,
        )
        .await?;
    let remembered = panel
        .window
        .update(cx, |v, _, _| v.remember)
        .map_err(|e| e.to_string())?;
    if !remembered {
        panel.button("save-remember", cx).await?;
    }
    panel.encoded(cx).await?;
    panel.button("save-submit", cx).await?;
    let path = match panel.completed(cx).await? {
        SaveOutcome::Saved { path, .. } => path,
        SaveOutcome::Cancelled => return Err("真实保存误返回取消".into()),
    };
    check_png(&std::fs::read(&path)?, &image)?;
    ensure(
        path.file_name().and_then(|s| s.to_str()) == Some("remember_128x96.png"),
        "保存后的文件名与生成名不同",
    )?;
    let stored = SaveStore::load();
    ensure(
        stored.config.filename_template == "remember_{width}x{height}"
            && stored.config.directory == qa.root.join("remembered"),
        "成功保存未记住命名规则和目录",
    )?;
    ensure(
        stored.recent_directories.first() == Some(&qa.root.join("remembered")),
        "成功保存没有更新目录 MRU",
    )?;
    let panel = open_panel(image.clone(), None, cx).await?;
    panel
        .window
        .update(cx, |v, _, _| {
            ensure(
                v.draft.filename_template == stored.config.filename_template
                    && &v.draft.directory
                        == stored
                            .recent_directories
                            .first()
                            .unwrap_or(&stored.config.directory),
                "新保存会话没有加载记住的规则",
            )
        })
        .map_err(|e| e.to_string())??;
    panel.button("save-format-bmp", cx).await?;
    panel.text("save-name", "bmp-white", cx).await?;
    let one_time_directory = qa.root.join("one-time-bmp");
    panel
        .text("save-directory", &one_time_directory.to_string_lossy(), cx)
        .await?;
    let remembered = panel
        .window
        .update(cx, |v, _, _| v.remember)
        .map_err(|e| e.to_string())?;
    if remembered {
        panel.button("save-remember", cx).await?;
    }
    panel.encoded(cx).await?;
    panel.button("save-submit", cx).await?;
    let path = match panel.completed(cx).await? {
        SaveOutcome::Saved { path, .. } => path,
        _ => return Err("BMP 保存没有完成".into()),
    };
    check_bmp(&std::fs::read(path)?, &image)?;
    ensure(
        SaveStore::load().config == stored.config,
        "不记住的会话修改了默认保存规则",
    )?;
    qa.pass("save_panel_png_bmp_success_remember_reload")?;

    let recent = SaveStore::load();
    ensure(
        recent.recent_directories.first() == Some(&one_time_directory),
        "不记住的成功保存没有更新最近目录",
    )?;
    let before = disk_snapshot(&qa.data)?;
    let panel = open_panel(image.clone(), None, cx).await?;
    panel
        .window
        .update(cx, |v, _, _| -> Result<()> {
            ensure(
                v.draft.directory == one_time_directory,
                "新保存面板没有优先使用最近保存目录",
            )?;
            ensure(
                v.draft.encoding == stored.config.encoding
                    && v.draft.filename_template == stored.config.filename_template,
                "最近目录错误地覆盖了已记住的格式或模板",
            )
        })
        .map_err(|e| e.to_string())??;
    panel.cancel(cx).await?;
    ensure(
        disk_snapshot(&qa.data)? == before,
        "取消从最近目录打开的面板修改了保存状态",
    )?;
    let history_before = crate::history::entries()?.len();
    let quick = crate::save_workflow::quick_save(&image, None, false)?;
    ensure(
        quick.path.parent() == Some(stored.config.directory.as_path()) && quick.path.is_file(),
        "最近保存目录错误地改变了快速保存默认目录",
    )?;
    ensure(
        SaveStore::load().config == stored.config
            && crate::history::entries()?.len() == history_before,
        "快速保存污染了默认规则或未开启的历史输出",
    )?;
    qa.pass("save_panel_recent_directory_independent_quick_default")?;

    let blocked = qa.root.join("ui-not-a-directory");
    std::fs::write(&blocked, b"own directory failure fixture")?;
    let before = disk_snapshot(&qa.data)?;
    let panel = open_panel(image.clone(), None, cx).await?;
    panel
        .text(
            "save-directory",
            &blocked.join("child").to_string_lossy(),
            cx,
        )
        .await?;
    panel.encoded(cx).await?;
    panel.button("save-submit", cx).await?;
    panel.failed_open("保存失败", cx).await?;
    ensure(
        disk_snapshot(&qa.data)? == before,
        "保存失败修改了配置、目录历史或截图历史",
    )?;
    panel.cancel(cx).await?;
    qa.pass("save_panel_export_failure_keeps_session")?;

    let directory = qa.root.join("collision");
    std::fs::create_dir_all(&directory)?;
    let occupied = directory.join("existing.png");
    std::fs::write(&occupied, b"own existing file must survive")?;
    let panel = open_panel(image.clone(), None, cx).await?;
    panel
        .text("save-directory", &directory.to_string_lossy(), cx)
        .await?;
    panel.text("save-name", "existing", cx).await?;
    panel.encoded(cx).await?;
    panel.button("save-submit", cx).await?;
    ensure(
        panel.outcome.borrow().is_none()
            && std::fs::read(&occupied)? == b"own existing file must survive",
        "同名提示前覆盖了原文件",
    )?;
    panel.button("save-overwrite-cancel", cx).await?;
    ensure(
        panel.outcome.borrow().is_none(),
        "取消覆盖提示结束了保存会话",
    )?;
    panel.button("save-submit", cx).await?;
    panel.button("save-keep-both", cx).await?;
    let path = match panel.completed(cx).await? {
        SaveOutcome::Saved { path, .. } => path,
        _ => return Err("保留两份没有保存新文件".into()),
    };
    ensure(
        path != occupied && std::fs::read(&occupied)? == b"own existing file must survive",
        "保留两份覆盖了原文件",
    )?;
    check_png(&std::fs::read(path)?, &image)?;
    qa.pass("save_panel_same_name_confirm_keep_both")?;

    let protected = directory.join("protected.png");
    let original = export::encode_image(&image, SaveEncoding::default())?;
    export::save_encoded(&original, &protected, false)?;
    let before = disk_snapshot(&qa.data)?;
    let panel = open_panel(image, Some(protected.clone()), cx).await?;
    panel
        .text("save-directory", &directory.to_string_lossy(), cx)
        .await?;
    panel.text("save-name", "protected.png", cx).await?;
    panel.encoded(cx).await?;
    panel.button("save-submit", cx).await?;
    panel.failed_open("保留原始图片", cx).await?;
    ensure(
        std::fs::read(&protected)? == original && disk_snapshot(&qa.data)? == before,
        "原图保护失败或错误后改写了状态",
    )?;
    panel.cancel(cx).await?;
    qa.pass("save_panel_protected_original_never_overwritten")?;

    owner_cases(fixture(), &mut qa, cx).await?;

    for _ in 0..10 {
        if cx
            .update(|cx| cx.windows().len())
            .map_err(|e| e.to_string())?
            == 1
        {
            break;
        }
        step(cx).await;
    }
    ensure(
        cx.update(|cx| cx.windows().len())
            .map_err(|e| e.to_string())?
            == 1,
        "保存专项遗留了面板窗口",
    )?;
    qa.pass("save_windows_recycled")?;
    Ok(())
}

struct QaHost;
impl Render for QaHost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_4()
            .child("RustShot 保存专项：仅合成图片，完成后自动关闭")
    }
}
/// 独立入口避免为保存验收冻结真实桌面；CLI 只需转发至此函数。
pub(crate) fn run() -> Result<()> {
    let mut qa = Qa::new()?;
    let image = fixture();
    let result = (|| -> Result<()> {
        let mut preferences = Preferences::load();
        preferences.history = true;
        preferences.quick_dir = qa.root.join("output");
        preferences.save()?;
        store_config(initial_config(&qa))?;
        encoding_cases(&image, &mut qa)?;
        quick_save_cases(&image, &mut qa)
    })();
    if let Err(error) = result {
        qa.emit("save_failed", &error.to_string(), false)?;
        return Err(error);
    }
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let errors = Rc::new(RefCell::new(Some("保存专项尚未完成".to_string())));
    let final_errors = errors.clone();
    let report = qa.root.join("ui.jsonl");
    let report_on_finish = report.clone();
    Application::new().with_assets(crate::toolbar_icons::IconAssets).run(move |cx: &mut App| {
        let host = cx.open_window(WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(400.), px(110.)), cx))),
            titlebar: Some(TitlebarOptions { title: Some("RustShot 保存专项（合成图）".into()), ..Default::default() }),
            ..Default::default()
        }, |_, cx| cx.new(|_| QaHost));
        let host = match host { Ok(host) => host, Err(error) => { *errors.borrow_mut() = Some(error.to_string()); cx.quit(); return; } };
        let done = Rc::new(Cell::new(false));
        let timeout_done = done.clone();
        let timeout_errors = errors.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(TIMEOUT).await;
            if !timeout_done.get() {
                *timeout_errors.borrow_mut() = Some("保存原生专项超时（75秒）".into());
                let _ = cx.update(close_owned_windows);
            }
        }).detach();
        cx.spawn(async move |cx| {
            step(cx).await;
            let result = verify(image, qa, cx).await;
            done.set(true);
            if let Err(error) = &result {
                let line = format!("{{\"suite\":\"save\",\"test\":\"save_failed\",\"pid\":{},\"error\":\"{}\",\"ok\":false}}", std::process::id(), escape(&error.to_string()));
                println!("{line}");
                if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&report_on_finish) { let _ = writeln!(file, "{line}"); }
            }
            *errors.borrow_mut() = result.err().map(|e| e.to_string());
            let _ = host.update(cx, |_, w, _| w.remove_window());
            let _ = cx.update(close_owned_windows);
        }).detach();
    });
    let error = final_errors.borrow_mut().take();
    if let Some(error) = error {
        return Err(error.into());
    }
    println!("{{\"phase\":\"save_smoke_complete\",\"pid\":{},\"report\":\"{}\",\"synthetic_only\":true,\"ok\":true}}", std::process::id(), escape(&report.to_string_lossy()));
    Ok(())
}
fn close_owned_windows(cx: &mut App) {
    // 此 App 是独立专项进程，窗口列表不包含任何用户常驻会话。
    for window in cx.windows() {
        let _ = cx.update_window(window, |_, w, _| w.remove_window());
    }
    cx.quit();
}
