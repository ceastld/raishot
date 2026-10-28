#[path = "gif_file_command.rs"]
mod gif_file_command;

use crate::{
    capture::{memory_mib, Snapshot},
    export,
    geometry::{Rect, Rotation},
    ppocr::OcrProfile,
    recognition::{layout::CopyLayoutMode, OcrProvider, RecognizedText},
    ui, Result,
};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

fn region(text: &str) -> Result<Rect> {
    let parts: Vec<u32> = text
        .split(',')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    if parts.len() != 4 {
        return Err("--region 格式应为 x,y,width,height，单位为显示器内的物理像素".into());
    }
    Ok(Rect {
        x: parts[0],
        y: parts[1],
        width: parts[2],
        height: parts[3],
    })
}

#[derive(Debug, PartialEq, Eq)]
enum OcrCommandKind {
    Recognize(PathBuf),
    Detect(PathBuf),
    Table(PathBuf),
    SelfTest,
}

#[derive(Debug, PartialEq, Eq)]
struct OcrCommand {
    provider: OcrProvider,
    profile: OcrProfile,
    copy_layout: Option<CopyLayoutMode>,
    kind: OcrCommandKind,
}

fn parse_ocr_command(args: &[String]) -> Result<Option<OcrCommand>> {
    if !args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--ocr-provider"
                | "--ocr-profile"
                | "--ocr-layout"
                | "--ocr-file"
                | "--detect-text-file"
                | "--table-file"
                | "--ocr-self-test"
                | "--ocr-self-test-ppocr"
        )
    }) {
        return Ok(None);
    }
    let mut provider = None;
    let mut profile = None;
    let mut copy_layout = None;
    let mut kind = None;
    let mut require_ppocr = false;
    let mut i = 0;
    while i < args.len() {
        let next_kind = match args[i].as_str() {
            "--ocr-profile" => {
                let value = args.get(i + 1).ok_or("--ocr-profile 缺少值")?;
                if profile.replace(OcrProfile::parse(value)?).is_some() {
                    return Err("--ocr-profile 不能重复指定".into());
                }
                i += 2;
                continue;
            }
            "--ocr-layout" => {
                let value = args.get(i + 1).ok_or("--ocr-layout 缺少值")?;
                if copy_layout.replace(CopyLayoutMode::parse(value)?).is_some() {
                    return Err("--ocr-layout 不能重复指定".into());
                }
                i += 2;
                continue;
            }
            "--ocr-provider" => {
                let value = args.get(i + 1).ok_or("--ocr-provider 缺少值")?;
                if provider.replace(OcrProvider::parse(value)?).is_some() {
                    return Err("--ocr-provider 不能重复指定".into());
                }
                i += 2;
                continue;
            }
            "--ocr-file" | "--detect-text-file" | "--table-file" => {
                let value = args.get(i + 1).ok_or("OCR 参数缺少图片路径")?;
                i += 1;
                if args[i - 1] == "--ocr-file" {
                    OcrCommandKind::Recognize(PathBuf::from(value))
                } else if args[i - 1] == "--table-file" {
                    OcrCommandKind::Table(PathBuf::from(value))
                } else {
                    OcrCommandKind::Detect(PathBuf::from(value))
                }
            }
            "--ocr-self-test" => OcrCommandKind::SelfTest,
            "--ocr-self-test-ppocr" => {
                require_ppocr = true;
                OcrCommandKind::SelfTest
            }
            other => return Err(format!("OCR 文件／自测入口不支持参数：{other}").into()),
        };
        if kind.replace(next_kind).is_some() {
            return Err("OCR 文件、检测文件、表格文件和 OCR 自测请分别运行".into());
        }
        i += 1;
    }
    let provider = provider.unwrap_or_default();
    let kind = kind.ok_or("--ocr-provider、--ocr-profile 或 --ocr-layout 请同时指定 OCR 入口")?;
    if profile.is_some() && matches!(kind, OcrCommandKind::SelfTest) {
        return Err(
            "--ocr-profile 仅支持 --ocr-file、--detect-text-file 或 --table-file，不支持自测"
                .into(),
        );
    }
    if profile.is_some() && provider != OcrProvider::Ppocr {
        return Err("--ocr-profile 仅支持 ppocr 提供者，Windows 模式不能指定模型".into());
    }
    if copy_layout.is_some() && !matches!(kind, OcrCommandKind::Recognize(_)) {
        return Err("--ocr-layout 仅支持 --ocr-file".into());
    }
    if provider != OcrProvider::Ppocr
        && (require_ppocr || matches!(kind, OcrCommandKind::Detect(_) | OcrCommandKind::Table(_)))
    {
        return Err("独立文字检测、表格识别和 --ocr-self-test-ppocr 仅使用 ppocr 提供者".into());
    }
    Ok(Some(OcrCommand {
        provider,
        profile: profile.unwrap_or(OcrProfile::Tiny),
        copy_layout,
        kind,
    }))
}

fn rect_json(rect: Rect) -> serde_json::Value {
    serde_json::json!({
        "x": rect.x,
        "y": rect.y,
        "width": rect.width,
        "height": rect.height,
    })
}

fn geometry_json(
    geometry: Option<crate::recognition::TextGeometry>,
    rect: Option<Rect>,
) -> Option<serde_json::Value> {
    let geometry = geometry?;
    let projection = geometry.validated_in(rect?)?;
    Some(serde_json::json!({
        "quad": geometry.quad.map(|point| [point.x, point.y]),
        "reading_direction": projection.direction().wire_name(),
        "precision": "estimated_text_band",
    }))
}

fn recognized_json(result: RecognizedText, provider: OcrProvider) -> serde_json::Value {
    let decisions = crate::ocr_result_filter::classify(&result);
    let baseline: Vec<_> = crate::ocr_result_filter::classify_baseline(&result)
        .into_iter()
        .enumerate()
        .filter_map(|(index, reason)| {
            reason.map(|reason| serde_json::json!({"index":index,"reason":reason.code()}))
        })
        .collect();
    let filtered: Vec<_> = decisions
        .iter()
        .enumerate()
        .filter_map(|(index, reason)| {
            reason.map(|reason| serde_json::json!({"index":index,"reason":reason.code()}))
        })
        .collect();
    serde_json::json!({
        "kind": "ocr",
        "provider": provider.name(),
        "text": result.text,
        "filter": {"algorithm":"contextual-symbol-v2", "enabled_by_default":false, "lines":filtered,"baseline_lines":baseline},
        "regions": result.regions.into_iter().map(rect_json).collect::<Vec<_>>(),
        "words": result.words.into_iter().map(|word| serde_json::json!({
            "text": word.text,
            "rect": rect_json(word.rect),
            "line": word.line,
            "horizontal_direction": word.horizontal_direction.wire_name(),
            "geometry": geometry_json(word.geometry, Some(word.rect)),
        })).collect::<Vec<_>>(),
        "lines": result.lines.into_iter().map(|line| serde_json::json!({
            "text": line.text,
            "rect": line.rect.map(rect_json),
            "horizontal_direction": line.horizontal_direction.wire_name(),
            "geometry": geometry_json(line.geometry, line.rect),
        })).collect::<Vec<_>>(),
    })
}

fn with_ocr_model_identity(
    mut value: serde_json::Value,
    provider: OcrProvider,
    profile: OcrProfile,
) -> serde_json::Value {
    if provider == OcrProvider::Ppocr {
        value["profile"] = profile.as_str().into();
        value["model"] = profile.provider_id().into();
    }
    value
}

fn detected_json(regions: Vec<Rect>, profile: OcrProfile) -> serde_json::Value {
    with_ocr_model_identity(
        serde_json::json!({
            "kind": "text_detection",
            "provider": "ppocr",
            "regions": regions.into_iter().map(rect_json).collect::<Vec<_>>(),
        }),
        OcrProvider::Ppocr,
        profile,
    )
}

/// CLI 与后台端到端诊断共用编码；没有可信结构时保留文字和警告。
pub(crate) fn table_json(
    result: &crate::ppocr::table::TableRecognition,
    profile: OcrProfile,
) -> Result<serde_json::Value> {
    let mut value = recognized_json(result.recognized.clone(), OcrProvider::Ppocr);
    value["kind"] = "table".into();
    value["model"] = "slanet-plus".into();
    value["profile"] = profile.as_str().into();
    value["ocr_model"] = profile.provider_id().into();
    value["source_lines"] = result.recognized.lines.len().into();
    value["region"] = serde_json::json!(&result.region);
    value["warning"] = serde_json::json!(&result.warning);
    for field in [
        "rows",
        "columns",
        "source",
        "confidence",
        "cells",
        "tsv",
        "html",
    ] {
        value[field] = serde_json::Value::Null;
    }
    if let Some(grid) = &result.grid {
        let tsv = crate::table_export::tsv(grid).ok_or("表格结构无效，无法输出 TSV")?;
        let html = crate::table_export::html(grid).ok_or("表格结构无效，无法输出 HTML")?;
        value["rows"] = grid.rows().into();
        value["columns"] = grid.columns().into();
        value["source"] = match grid.source {
            crate::table_grid::TableSource::Lines => "lines",
            crate::table_grid::TableSource::Alignment => "alignment",
            crate::table_grid::TableSource::Legacy => "legacy",
            crate::table_grid::TableSource::Model => "model",
        }
        .into();
        value["confidence"] = serde_json::json!(grid.confidence);
        value["cells"] = serde_json::json!(grid
            .cells
            .iter()
            .map(|cell| serde_json::json!({
                "row": cell.row,
                "column": cell.column,
                "rowspan": cell.rowspan,
                "colspan": cell.colspan,
                "text": cell.text,
                "rect": rect_json(cell.rect),
            }))
            .collect::<Vec<_>>());
        value["tsv"] = tsv.into();
        value["html"] = html.into();
    }
    Ok(value)
}

fn recognized_copy_json(
    result: &RecognizedText,
    mode: CopyLayoutMode,
) -> Result<serde_json::Value> {
    let layout = result.copy_layout();
    let copied = layout.copy_mapped(0..layout.text().len(), mode)?;
    Ok(serde_json::json!({
        "mode": mode.wire_name(),
        "text": copied.text,
        "source_range": [copied.source_range.start, copied.source_range.end],
        "spans": copied.spans.into_iter().map(|span| serde_json::json!({
            "source_range": [span.source_range.start, span.source_range.end],
            "output_range": [span.output_range.start, span.output_range.end],
            "kind": match span.kind {
                crate::recognition::layout::CopySpanKind::Verbatim => "verbatim",
                crate::recognition::layout::CopySpanKind::LineJoin => "line_join",
            },
        })).collect::<Vec<_>>(),
    }))
}

fn run_ocr_command(command: OcrCommand) -> Result<()> {
    match command.kind {
        OcrCommandKind::SelfTest => {
            crate::recognition::self_test_with_provider(command.provider)?;
        }
        OcrCommandKind::Recognize(path) => {
            let image = export::read_image(&path)?;
            let owner = crate::recognition::task::CancelOwner::new();
            let result = crate::recognition::recognize_with_profile_cancellable(
                image,
                command.provider,
                command.profile,
                &owner.token(),
            )?;
            let copy = command
                .copy_layout
                .map(|mode| recognized_copy_json(&result, mode))
                .transpose()?;
            let mut value = with_ocr_model_identity(
                recognized_json(result, command.provider),
                command.provider,
                command.profile,
            );
            if let Some(copy) = copy {
                value["copy"] = copy;
            }
            println!("{value}");
        }
        OcrCommandKind::Detect(path) => {
            let image = export::read_image(&path)?;
            let owner = crate::recognition::task::CancelOwner::new();
            let regions = crate::recognition::detect_regions_with_profile_cancellable(
                image,
                command.profile,
                &owner.token(),
            )?;
            println!("{}", detected_json(regions, command.profile));
        }
        OcrCommandKind::Table(path) => {
            let image = export::read_image(&path)?;
            let owner = crate::recognition::task::CancelOwner::new();
            let result = crate::ppocr::table::recognize_cancellable(
                &image,
                command.profile,
                &owner.token(),
            )?;
            println!("{}", table_json(&result, command.profile)?);
        }
    }
    Ok(())
}

pub fn run() -> Result<()> {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--media-playback-smoke") {
        if args.len() != 2
            || args[0] != "--media-playback-smoke"
            || args[1].is_empty()
            || args[1].starts_with("--")
        {
            return Err("用法：--media-playback-smoke <合成媒体输出目录>，请单独运行".into());
        }
        let report = crate::media_playback_smoke::run(Path::new(&args[1]))?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        if report.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err("合成媒体编解码自检失败，详见 JSON 中的阶段错误".into());
        }
        return Ok(());
    }
    if !args.iter().any(|arg| arg == "--help" || arg == "-h")
        && crate::file_launch_command::execute_cli(&args)?
    {
        return Ok(());
    }
    if gif_file_command::execute(&args)? {
        return Ok(());
    }
    let ocr_command = if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        None
    } else {
        parse_ocr_command(&args)?
    };
    if let Some(command) = ocr_command {
        return run_ocr_command(command);
    }
    if args == ["--ui-smoke"] || args == ["--resident-smoke"] {
        std::env::set_var(
            "RUSTSHOT_DATA_DIR",
            std::env::current_dir()?.join("target/qa/settings"),
        );
    }
    if args.len() == 2 && args[0] == "--qr-self-test" {
        let image = export::read_image(Path::new(&args[1]))?;
        let result = crate::recognition::qr(image)?;
        println!("二维码识别：{result}");
        return Ok(());
    }
    if args == ["--pin-smoke"] {
        std::env::set_var("RUSTSHOT_DATA_DIR", "target/qa/pin/settings");
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg("--pin-smoke")
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("贴图专项失败".into())
        };
    }
    if args == ["--fast-capture-smoke"] {
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg("--fast-capture-smoke")
        .env("RUSTSHOT_DATA_DIR", "target/qa/fast-capture/settings")
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("GDI 冻结色块专项失败".into())
        };
    }
    if args == ["--path-resource-smoke"] {
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg("--path-resource-smoke")
        .env("RUSTSHOT_STARTUP_TRACE", "1")
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("路径资源首次绘制/重建专项失败".into())
        };
    }
    if args == ["--startup-surface-smoke"] {
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg("--startup-surface-smoke")
        .env("RUSTSHOT_QA_NO_RESIDENT_NOTIFY", "1")
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("快速框选首屏专项失败".into())
        };
    }
    if args == ["--hotkey-self-test"] {
        return crate::hotkey_registration_smoke::run();
    }
    if args == ["--pin-workspace-smoke"] {
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg("--pin-workspace-smoke")
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("贴图工作区专项失败".into())
        };
    }
    if args == ["--save-smoke"] {
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg("--save-smoke")
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("保存工作流专项失败".into())
        };
    }
    if args == ["--customization-smoke"] {
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg("--customization-smoke")
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("快捷键/工具栏自定义专项失败".into())
        };
    }
    if args == ["--selection-finish-smoke"] {
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg("--selection-finish-smoke")
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("选区外观专项失败".into())
        };
    }
    if args == ["--startup-probe"] {
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg("--startup-probe")
        .env("RUSTSHOT_DATA_DIR", "target/qa/startup/settings")
        .env("RUSTSHOT_STARTUP_TRACE", "1")
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("截图启动专项失败".into())
        };
    }
    if args == ["--mask-self-test"] {
        return crate::mask_quality::run();
    }
    if args == ["--vector-self-test"] {
        return crate::vector_quality::run();
    }
    if args == ["--vector-aa-smoke"] || args == ["--rotation-smoke"] {
        let rotation = args[0] == "--rotation-smoke";
        std::env::set_var(
            "RUSTSHOT_DATA_DIR",
            if rotation {
                "target/qa/rotation/settings"
            } else {
                "target/qa/vector-aa/settings"
            },
        );
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg(&args[0])
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err(if rotation {
                "旋转把手专项失败"
            } else {
                "矢量抗锯齿专项失败"
            }
            .into())
        };
    }
    if args == ["--pencil-self-test"] {
        return crate::pencil_quality::run();
    }
    if args == ["--long-self-test"] {
        return crate::long_smoke::run();
    }
    if args == ["--record-self-test"] {
        return crate::recording::self_test();
    }
    if args == ["--record-codec-benchmark"] {
        return crate::recording_codec_benchmark::run();
    }
    if args == ["--recording-ui-smoke"] {
        let status = std::process::Command::new(
            std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE),
        )
        .arg(&args[0])
        .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("录屏界面冒烟失败，详见标准错误输出".into())
        };
    }
    if args == ["--ocr-self-test"] {
        return crate::recognition::self_test();
    }
    if args == ["--idle-probe"] {
        return ui::idle_probe();
    }
    if args == ["--resident-smoke"] {
        return ui::resident_smoke();
    }
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{name} {}\n\n  {resident}                         托盘待机，默认 Ctrl+1 截图 / Ctrl+2 剪贴板贴图（可在设置修改）\n  {cli} --capture               交互截图一次\n  {cli} --output shot.png       截取鼠标所在显示器\n    [--region x,y,width,height]        仅导出指定选区\n    [--all-screens]                    导出全部屏幕；坐标相对于虚拟桌面原点\n  {cli} --pin-file image.png [--at x,y]  直接贴图；位置为屏幕物理像素\n  {cli} --edit-file image.png   直接打开图片编辑器\n  {cli} --pin-clipboard [--at x,y]  图片或纯文本剪贴板贴图\n  {cli} --benchmark 5           测量 5 次冷启动截图和裁剪\n    [--output shot.png]                包含 PNG 编码与写盘测量\n  {cli} --vector-self-test      合成曲线箭头与可编辑折线样图\n  {cli} --pencil-self-test      合成铅笔矢量/导出图及算法耗时\n  {cli} --ui-smoke              打开预览，验证双屏/标注/工具栏后自动退出\n  {cli} --selection-finish-smoke 验证选区圆角/阴影/边框和导出后自动退出\n  {cli} --startup-probe         验证首帧、原生取消和资源回收\n  {cli} --hotkey-self-test      验证原生热键冲突、切换和清理\n  {cli} --customization-smoke   验证快捷键录制及截图/贴图工具栏配置\n  {cli} --self-test             验证真实 GPU 裁剪/旋转和 PNG 色彩\n  {cli} --ocr-file image.png    文件 OCR，输出文字和原图视觉行／词框 JSON\n    [--ocr-provider ppocr|windows]     默认 ppocr；Windows 为显式独立提供者\n    [--ocr-profile tiny|small]         文件 OCR/文字检测选择模型，默认 tiny；仅 ppocr\n    [--ocr-layout original|auto]       追加复制排版及原文范围映射；默认保留原文\n  {cli} --detect-text-file image.png  独立 PP-OCR 文字检测，只输出框 JSON\n  {cli} --ocr-self-test-ppocr   Rust 进程内 PP-OCR 后台自测\n  {cli} --ocr-self-test [--ocr-provider ppocr|windows]  指定提供者后台自测\n  {cli} --record-codec-benchmark 合成画面编解码对比（不读取桌面）\n  {cli} --convert-gif input.mp4 --output result.gif [--gif-fps 15]  转为GIF并输出阶段耗时\n  {cli} --record-self-test      DXGI 录屏、暂停、MF 编码 MP4 与 GIF 转换
  {cli} --recording-ui-smoke    录屏工具栏→录制→框内预览→关闭（不抢焦点）\n  {cli} --qr-self-test image.png 二维码识别\n", env!("CARGO_PKG_VERSION"), name = crate::brand::NAME, resident = crate::brand::RESIDENT_EXE, cli = crate::brand::CLI_EXE.trim_end_matches(".exe"));
        println!("  {cli} --table-file image.png [--ocr-profile tiny|small]\n    独立 SLANet_plus 表格识别，首次按需下载模型；输出单元格、跨度、TSV、HTML 及原文 JSON\n    仅 ppocr；未检测到可信表格时保留文字和警告，不接受 --ocr-layout\n", cli = crate::brand::CLI_EXE.trim_end_matches(".exe"));
        return Ok(());
    }
    let mut output: Option<PathBuf> = None;
    let mut selected = None;
    let mut benchmark = None;
    let mut capture = false;
    let mut quick = None;
    let mut smoke = false;
    let mut self_test = false;
    let mut all_screens = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--output" | "--region" | "--benchmark" => {
                let value = args.get(i + 1).ok_or("参数缺少值")?;
                match args[i].as_str() {
                    "--output" => output = Some(PathBuf::from(value)),
                    "--region" => selected = Some(region(value)?),
                    _ => {
                        let n: u32 = value.parse()?;
                        if !(1..=100).contains(&n) {
                            return Err("次数须为 1..100".into());
                        }
                        benchmark = Some(n);
                    }
                }
                i += 1;
            }
            "--capture" => capture = true,
            "--action" => {
                let id = args.get(i + 1).ok_or("--action 缺少动作 ID")?;
                quick = Some(
                    crate::quick_action_store::QuickActionStore::load()
                        .resolve(id)?
                        .pack()?,
                );
                i += 1;
            }
            "--ui-smoke" => smoke = true,
            "--self-test" => self_test = true,
            "--all-screens" => all_screens = true,
            other => return Err(format!("未知参数：{other}").into()),
        }
        i += 1;
    }
    let interactive_modes = usize::from(capture) + usize::from(smoke) + usize::from(self_test);
    if quick.is_some() && (!capture || smoke || self_test || all_screens) {
        return Err("--action 仅与 --capture 一起使用；采集来源由动作决定".into());
    }
    if interactive_modes > 1
        || (interactive_modes > 0
            && (output.is_some() || selected.is_some() || benchmark.is_some()))
    {
        return Err("--capture、--ui-smoke、--self-test 请单独运行".into());
    }
    if capture || smoke {
        let worker = std::env::current_exe()?.with_file_name(crate::brand::GPUI_EXE);
        let status = std::process::Command::new(worker)
            .args(quick.map_or_else(Vec::new, |code| {
                vec!["--quick".to_string(), format!("0x{code:08X}")]
            }))
            .arg(if smoke {
                "--smoke"
            } else if all_screens {
                "--all-screens"
            } else {
                "--region"
            })
            .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err("GPUI 截图工作进程失败".into())
        };
    }
    if self_test {
        return gpu_self_test();
    }
    if all_screens {
        let path = output
            .as_ref()
            .ok_or("--all-screens 请同时指定 --output 或 --capture")?;
        if benchmark.is_some() {
            return Err("多屏导出暂不支持 --benchmark".into());
        }
        let desktop = crate::desktop::DesktopCapture::capture()?;
        let rect = selected.unwrap_or_else(|| desktop.full());
        let image = desktop.read_region(rect)?;
        let (w, p) = memory_mib();
        println!("{{\"phase\":\"all_screens\",\"screens\":{},\"origin\":\"{},{}\",\"desktop\":\"{}x{}\",\"region\":\"{}x{}\",\"working_set_mib\":{w:.2},\"private_mib\":{p:.2}}}",desktop.screens.len(),desktop.bounds.left,desktop.bounds.top,desktop.bounds.width,desktop.bounds.height,rect.width,rect.height);
        return export::save_png(image, path);
    }
    if output.is_none() && benchmark.is_none() {
        return Err("请指定 --output 或 --benchmark".into());
    }
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    let (idle_w, idle_p) = memory_mib();
    println!(
        "{{\"phase\":\"before_gpu\",\"working_set_mib\":{idle_w:.2},\"private_mib\":{idle_p:.2}}}"
    );
    let count = benchmark.unwrap_or(1);
    let mut times = Vec::new();
    for n in 0..count {
        let start = Instant::now();
        let snapshot = Snapshot::capture()?;
        let rect = selected.unwrap_or_else(|| {
            if benchmark.is_some() {
                Rect {
                    x: 0,
                    y: 0,
                    width: snapshot.width.min(800),
                    height: snapshot.height.min(600),
                }
            } else {
                Rect::full(snapshot.width, snapshot.height)
            }
        });
        let image = snapshot.read_region(rect)?;
        let readback_ms = image.readback_ms;
        let (active_w, active_p) = memory_mib();
        let (gpu, nonlocal) = snapshot.gpu_bytes().unwrap_or((0, 0));
        let encode = Instant::now();
        let save_ms = if let Some(path) = output.as_ref() {
            export::save_png(image, path)?;
            encode.elapsed().as_secs_f64() * 1000.0
        } else {
            drop(image);
            0.0
        };
        let capture_ms = snapshot.capture_ms;
        let adapter = snapshot.adapter_name.clone();
        let (width, height) = (snapshot.width, snapshot.height);
        drop(snapshot);
        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        times.push(total_ms);
        let (after_w, after_p) = memory_mib();
        println!("{{\"iteration\":{},\"adapter\":{:?},\"screen\":\"{}x{}\",\"region\":\"{}x{}\",\"capture_ms\":{capture_ms:.3},\"readback_ms\":{readback_ms:.3},\"png_ms\":{save_ms:.3},\"total_ms\":{total_ms:.3},\"active_working_set_mib\":{active_w:.2},\"active_private_mib\":{active_p:.2},\"gpu_local_mib\":{:.2},\"gpu_nonlocal_mib\":{:.2},\"after_working_set_mib\":{after_w:.2},\"after_private_mib\":{after_p:.2}}}",
            n + 1, adapter, width, height, rect.width, rect.height, gpu as f64 / 1048576.0, nonlocal as f64 / 1048576.0);
    }
    times.sort_by(f64::total_cmp);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let median = times[times.len() / 2];
    let p95 = times[(times.len() * 95).div_ceil(100).saturating_sub(1)];
    println!("{{\"summary\":true,\"count\":{count},\"total_mean_ms\":{mean:.3},\"total_median_ms\":{median:.3},\"total_p95_ms\":{p95:.3}}}");
    Ok(())
}

fn gpu_self_test() -> Result<()> {
    let mut snapshot = Snapshot::capture()?;
    let (rw, rh) = (snapshot.raw_width, snapshot.raw_height);
    let color = |x: u32, y: u32| {
        [
            (x % 251) as u8,
            (y % 241) as u8,
            ((x + y) % 239) as u8,
            255u8,
        ]
    };
    let mut pattern = vec![0u8; rw as usize * rh as usize * 4];
    for y in 0..rh {
        for x in 0..rw {
            let p = ((y * rw + x) * 4) as usize;
            pattern[p..p + 4].copy_from_slice(&color(x, y));
        }
    }
    unsafe {
        snapshot.context.UpdateSubresource(
            snapshot.texture(),
            0,
            None,
            pattern.as_ptr().cast(),
            rw * 4,
            0,
        );
    }
    drop(pattern);
    for rotation in [
        Rotation::Identity,
        Rotation::R90,
        Rotation::R180,
        Rotation::R270,
    ] {
        snapshot.rotation = rotation;
        (snapshot.width, snapshot.height) = rotation.size(rw, rh);
        let r = Rect {
            x: 7,
            y: 11,
            width: 83,
            height: 57,
        };
        let image = snapshot.read_region(r)?;
        for y in 0..r.height {
            for x in 0..r.width {
                let (rx, ry) = rotation.raw_pixel(r.x + x, r.y + y, rw, rh);
                let p = ((y * r.width + x) * 4) as usize;
                if image.bgra[p..p + 4] != color(rx, ry) {
                    return Err(format!("GPU 裁剪色彩校验失败：{rotation:?} ({x}, {y})").into());
                }
            }
        }
        let path = Path::new("target/qa/gpu-roundtrip.png");
        std::fs::create_dir_all(path.parent().unwrap())?;
        let first = image.bgra[..4].to_vec();
        export::save_png(image, path)?;
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
        let mut reader = decoder.read_info()?;
        let mut decoded = vec![0; reader.output_buffer_size().ok_or("PNG 解码尺寸未知")?];
        let info = reader.next_frame(&mut decoded)?;
        if info.width != r.width
            || info.height != r.height
            || decoded[..4] != [first[2], first[1], first[0], 255]
        {
            return Err("PNG 尺寸或 BGRA/RGBA 校验失败".into());
        }
        println!(
            "{{\"test\":\"gpu_crop_png\",\"rotation\":{:?},\"ok\":true}}",
            format!("{rotation:?}")
        );
    }
    if snapshot
        .read_region(Rect {
            x: u32::MAX,
            y: 0,
            width: 20,
            height: 10,
        })
        .is_ok()
    {
        return Err("越界选区未被拒绝".into());
    }
    Ok(())
}

#[cfg(test)]
mod ocr_command_tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<OcrCommand>> {
        parse_ocr_command(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn file_profile_defaults_tiny_and_explicit_profiles_route_only_file_inputs() {
        for (name, profile) in [("tiny", OcrProfile::Tiny), ("small", OcrProfile::Small)] {
            for entry in ["--ocr-file", "--detect-text-file"] {
                let first = parse(&["--ocr-profile", name, entry, "input.png"])
                    .unwrap()
                    .unwrap();
                let last = parse(&[
                    entry,
                    "input.png",
                    "--ocr-provider",
                    "ppocr",
                    "--ocr-profile",
                    name,
                ])
                .unwrap()
                .unwrap();
                assert_eq!(first, last);
                assert_eq!(first.provider, OcrProvider::Ppocr);
                assert_eq!(first.profile, profile);
                assert_eq!(first.copy_layout, None);
                let default = parse(&[entry, "input.png"]).unwrap().unwrap();
                assert_eq!(default.profile, OcrProfile::Tiny);
            }
        }
        let layout = parse(&[
            "--ocr-file",
            "input.png",
            "--ocr-profile",
            "small",
            "--ocr-layout",
            "auto",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(layout.profile, OcrProfile::Small);
        assert_eq!(layout.copy_layout, Some(CopyLayoutMode::Auto));
    }
    #[test]
    fn explicit_profile_rejects_windows_self_tests_missing_invalid_and_duplicate_values() {
        for profile in ["tiny", "small"] {
            for test in ["--ocr-self-test", "--ocr-self-test-ppocr"] {
                let error = parse(&[test, "--ocr-profile", profile])
                    .unwrap_err()
                    .to_string();
                assert!(error.contains("不支持自测"), "{error}");
            }
            let error = parse(&[
                "--ocr-provider",
                "windows",
                "--ocr-file",
                "input.png",
                "--ocr-profile",
                profile,
            ])
            .unwrap_err()
            .to_string();
            assert!(error.contains("Windows 模式不能指定模型"), "{error}");
        }
        for args in [
            vec!["--ocr-profile"],
            vec!["--ocr-profile", "small"],
            vec!["--ocr-profile", "", "--ocr-file", "input.png"],
            vec!["--ocr-profile", "Small", "--ocr-file", "input.png"],
            vec!["--ocr-profile", "unknown", "--ocr-file", "input.png"],
            vec![
                "--ocr-profile",
                "tiny",
                "--ocr-profile",
                "small",
                "--ocr-file",
                "input.png",
            ],
            vec!["--ocr-profile", "small", "--capture"],
            vec!["--ocr-profile", "small", "--qr-self-test", "input.png"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        assert!(parse(&["--ocr-profile"])
            .unwrap_err()
            .to_string()
            .contains("缺少值"));
        assert!(parse(&[
            "--ocr-file",
            "input.png",
            "--ocr-profile",
            "small",
            "--ocr-profile",
            "small"
        ])
        .unwrap_err()
        .to_string()
        .contains("不能重复指定"));
    }
    #[test]
    fn recognition_model_metadata_keeps_original_json_and_never_labels_windows_ppocr() {
        let result = || RecognizedText {
            text: "中文👩‍🔬\r\nsecond".into(),
            regions: vec![Rect {
                x: 12,
                y: 9,
                width: 70,
                height: 16,
            }],
            words: Vec::new(),
            lines: Vec::new(),
        };
        let base = recognized_json(result(), OcrProvider::Ppocr);
        for (profile, id, model) in [
            (OcrProfile::Tiny, "tiny", "ppocrv6-tiny"),
            (OcrProfile::Small, "small", "ppocrv6-small"),
        ] {
            let mut value = with_ocr_model_identity(base.clone(), OcrProvider::Ppocr, profile);
            assert_eq!(value["profile"], id);
            assert_eq!(value["model"], model);
            let readback: serde_json::Value = serde_json::from_str(&value.to_string()).unwrap();
            assert_eq!(readback["text"], base["text"]);
            value.as_object_mut().unwrap().remove("profile");
            value.as_object_mut().unwrap().remove("model");
            assert_eq!(value, base);
        }
        let windows = recognized_json(result(), OcrProvider::Windows);
        for profile in [OcrProfile::Tiny, OcrProfile::Small] {
            let value = with_ocr_model_identity(windows.clone(), OcrProvider::Windows, profile);
            assert_eq!(value, windows);
            assert_eq!(value["provider"], "windows");
            assert!(value.get("profile").is_none());
            assert!(value.get("model").is_none());
        }
    }
    #[test]
    fn detection_model_metadata_preserves_roi_boxes_without_recognition_fields() {
        let rect = Rect {
            x: 31,
            y: 9,
            width: 50,
            height: 12,
        };
        for profile in [OcrProfile::Tiny, OcrProfile::Small] {
            let mut value = detected_json(vec![rect], profile);
            assert_eq!(value["profile"], profile.as_str());
            assert_eq!(value["model"], profile.provider_id());
            assert!(value.get("text").is_none());
            assert!(value.get("words").is_none());
            assert!(value.get("lines").is_none());
            value.as_object_mut().unwrap().remove("profile");
            value.as_object_mut().unwrap().remove("model");
            assert_eq!(
                value,
                serde_json::json!({"kind": "text_detection", "provider": "ppocr", "regions": [rect_json(rect)]})
            );
        }
    }

    #[test]
    fn file_ocr_defaults_to_ppocr_and_windows_is_an_explicit_separate_provider() {
        assert_eq!(
            parse(&["--ocr-file", "input.png"]).unwrap(),
            Some(OcrCommand {
                provider: OcrProvider::Ppocr,
                profile: OcrProfile::Tiny,
                copy_layout: None,
                kind: OcrCommandKind::Recognize(PathBuf::from("input.png")),
            }),
        );
        assert_eq!(
            parse(&["--ocr-provider", "windows", "--ocr-file", "input.png"]).unwrap(),
            Some(OcrCommand {
                provider: OcrProvider::Windows,
                profile: OcrProfile::Tiny,
                copy_layout: None,
                kind: OcrCommandKind::Recognize(PathBuf::from("input.png")),
            }),
        );
        assert_eq!(
            parse(&["--ocr-self-test-ppocr"]).unwrap(),
            Some(OcrCommand {
                provider: OcrProvider::Ppocr,
                profile: OcrProfile::Tiny,
                copy_layout: None,
                kind: OcrCommandKind::SelfTest,
            }),
        );
        assert_eq!(
            parse(&["--ocr-self-test", "--ocr-provider", "windows"]).unwrap(),
            Some(OcrCommand {
                provider: OcrProvider::Windows,
                profile: OcrProfile::Tiny,
                copy_layout: None,
                kind: OcrCommandKind::SelfTest,
            }),
        );
        assert!(parse(&["--output", "shot.png"]).unwrap().is_none());
    }

    #[test]
    fn independent_detection_and_invalid_ocr_arguments_cannot_fall_back_or_capture() {
        assert_eq!(
            parse(&["--detect-text-file", "input.png"]).unwrap(),
            Some(OcrCommand {
                provider: OcrProvider::Ppocr,
                profile: OcrProfile::Tiny,
                copy_layout: None,
                kind: OcrCommandKind::Detect(PathBuf::from("input.png")),
            }),
        );
        for args in [
            vec![
                "--ocr-provider",
                "windows",
                "--detect-text-file",
                "input.png",
            ],
            vec!["--ocr-self-test-ppocr", "--ocr-provider", "windows"],
            vec!["--ocr-provider", "auto", "--ocr-file", "input.png"],
            vec!["--ocr-file"],
            vec!["--ocr-provider", "windows"],
            vec!["--ocr-file", "input.png", "--capture"],
            vec!["--ocr-file", "input.png", "--detect-text-file", "input.png"],
            vec![
                "--ocr-self-test",
                "--ocr-provider",
                "ppocr",
                "--ocr-provider",
                "ppocr",
            ],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn explicit_copy_layout_is_separate_from_default_original_file_json() {
        for (name, mode) in [
            ("original", CopyLayoutMode::Original),
            ("auto", CopyLayoutMode::Auto),
        ] {
            assert_eq!(
                parse(&["--ocr-layout", name, "--ocr-file", "input.png"]).unwrap(),
                Some(OcrCommand {
                    provider: OcrProvider::Ppocr,
                    profile: OcrProfile::Tiny,
                    copy_layout: Some(mode),
                    kind: OcrCommandKind::Recognize(PathBuf::from("input.png")),
                }),
            );
        }
        assert_eq!(CopyLayoutMode::default(), CopyLayoutMode::Original);
        for args in [
            vec!["--ocr-layout"],
            vec!["--ocr-layout", "auto"],
            vec!["--ocr-layout", "Auto", "--ocr-file", "input.png"],
            vec!["--ocr-layout", "auto", "--ocr-self-test"],
            vec![
                "--ocr-layout",
                "original",
                "--detect-text-file",
                "input.png",
            ],
            vec![
                "--ocr-layout",
                "original",
                "--ocr-layout",
                "auto",
                "--ocr-file",
                "input.png",
            ],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn copy_json_keeps_original_text_and_geometry_with_separate_byte_mapping() {
        let first = Rect {
            x: 20,
            y: 10,
            width: 200,
            height: 20,
        };
        let second = Rect { y: 36, ..first };
        let result = RecognizedText {
            text: "alpha\nbeta".into(),
            regions: vec![first, second],
            words: Vec::new(),
            lines: vec![
                crate::recognition::RecognizedLine {
                    graphemes: None,
                    text: "alpha".into(),
                    rect: Some(first),
                    horizontal_direction: Default::default(),
                    geometry: None,
                },
                crate::recognition::RecognizedLine {
                    graphemes: None,
                    text: "beta".into(),
                    rect: Some(second),
                    horizontal_direction: Default::default(),
                    geometry: None,
                },
            ],
        };
        let original = recognized_copy_json(&result, CopyLayoutMode::Original).unwrap();
        assert_eq!(original["mode"], "original");
        assert_eq!(original["text"], "alpha\nbeta");
        assert_eq!(original["spans"].as_array().unwrap().len(), 1);
        let auto = recognized_copy_json(&result, CopyLayoutMode::Auto).unwrap();
        assert_eq!(auto["mode"], "auto");
        assert_eq!(auto["text"], "alpha beta");
        assert_eq!(auto["source_range"], serde_json::json!([0, 10]));
        assert_eq!(auto["spans"][1]["kind"], "line_join");
        assert_eq!(auto["spans"][1]["source_range"], serde_json::json!([5, 6]));
        let value = recognized_json(result, OcrProvider::Ppocr);
        assert_eq!(value["text"], "alpha\nbeta");
        assert_eq!(value["lines"][0]["rect"], rect_json(first));
        assert_eq!(value["lines"][1]["rect"], rect_json(second));
        assert!(value.get("copy").is_none());
    }

    #[test]
    fn file_result_json_preserves_visual_lines_utf8_and_roi_local_word_geometry() {
        let rect = Rect {
            x: 12,
            y: 9,
            width: 70,
            height: 16,
        };
        let value = recognized_json(
            RecognizedText {
                text: "中文\"\nsecond".into(),
                regions: vec![rect],
                words: vec![crate::recognition::RecognizedWord {
                    horizontal_direction: crate::recognition::HorizontalDirection::RightToLeft,
                    geometry: None,
                    text: "中文\"".into(),
                    rect,
                    line: 0,
                }],
                lines: vec![
                    crate::recognition::RecognizedLine {
                        graphemes: None,
                        horizontal_direction: crate::recognition::HorizontalDirection::RightToLeft,
                        geometry: None,
                        text: "中文\"".into(),
                        rect: Some(rect),
                    },
                    crate::recognition::RecognizedLine {
                        graphemes: None,
                        horizontal_direction: Default::default(),
                        geometry: None,
                        text: "second".into(),
                        rect: None,
                    },
                ],
            },
            OcrProvider::Ppocr,
        );
        let readback: serde_json::Value = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(readback["provider"], "ppocr");
        assert_eq!(readback["text"], "中文\"\nsecond");
        assert_eq!(readback["regions"][0], rect_json(rect));
        assert_eq!(readback["words"][0]["rect"], rect_json(rect));
        assert_eq!(readback["words"][0]["line"], 0);
        assert_eq!(readback["words"][0]["horizontal_direction"], "rtl");
        assert_eq!(readback["lines"][0]["rect"], rect_json(rect));
        assert!(readback["lines"][1]["rect"].is_null());
        assert_eq!(readback["lines"][0]["horizontal_direction"], "rtl");
        assert_eq!(readback["lines"][1]["horizontal_direction"], "ltr");
        assert!(readback["words"][0]["geometry"].is_null());
        assert!(readback["lines"][0]["geometry"].is_null());
    }

    #[test]
    fn projected_json_preserves_reading_order_and_declares_estimated_precision() {
        use crate::{annotation::Point, recognition::TextGeometry};
        let rect = Rect {
            x: 10,
            y: 20,
            width: 20,
            height: 100,
        };
        let geometry = TextGeometry {
            quad: [
                Point { x: 30., y: 20. },
                Point { x: 30., y: 120. },
                Point { x: 10., y: 120. },
                Point { x: 10., y: 20. },
            ],
        };
        let json = geometry_json(Some(geometry), Some(rect)).unwrap();
        assert_eq!(
            json["quad"],
            serde_json::json!([[30., 20.], [30., 120.], [10., 120.], [10., 20.]])
        );
        assert_eq!(json["reading_direction"], "ttb");
        assert_eq!(json["precision"], "estimated_text_band");
        assert!(geometry_json(Some(geometry), None).is_none());
        assert!(geometry_json(None, Some(rect)).is_none());
        assert!(geometry_json(Some(geometry), Some(Rect { height: 99, ..rect })).is_none());
    }
}

#[cfg(test)]
mod table_command_tests {
    use super::*;
    use crate::{
        ppocr::table::TableRecognition,
        table_grid::{TableCell, TableGrid, TableSource},
    };

    fn parse(args: &[&str]) -> Result<Option<OcrCommand>> {
        parse_ocr_command(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn table_file_defaults_tiny_and_accepts_explicit_ocr_profiles() {
        for (args, profile) in [
            (vec!["--table-file", "input.png"], OcrProfile::Tiny),
            (
                vec!["--table-file", "input.png", "--ocr-profile", "tiny"],
                OcrProfile::Tiny,
            ),
            (
                vec![
                    "--ocr-profile",
                    "small",
                    "--table-file",
                    "input.png",
                    "--ocr-provider",
                    "ppocr",
                ],
                OcrProfile::Small,
            ),
        ] {
            assert_eq!(
                parse(&args).unwrap(),
                Some(OcrCommand {
                    provider: OcrProvider::Ppocr,
                    profile,
                    copy_layout: None,
                    kind: OcrCommandKind::Table(PathBuf::from("input.png")),
                })
            );
        }
    }

    #[test]
    fn table_file_rejects_windows_copy_layout_missing_path_and_mixed_entries() {
        for args in [
            vec!["--table-file"],
            vec!["--table-file", "input.png", "--ocr-provider", "windows"],
            vec!["--table-file", "input.png", "--ocr-layout", "auto"],
            vec!["--table-file", "input.png", "--ocr-layout", "original"],
            vec!["--table-file", "input.png", "--ocr-profile", "unknown"],
            vec!["--table-file", "input.png", "--table-file", "other.png"],
            vec!["--table-file", "input.png", "--ocr-file", "other.png"],
            vec![
                "--table-file",
                "input.png",
                "--detect-text-file",
                "other.png",
            ],
            vec!["--table-file", "input.png", "--ocr-self-test"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    fn result() -> TableRecognition {
        let mut grid = TableGrid {
            row_edges: vec![0, 20, 40],
            column_edges: vec![0, 40, 100, 160],
            cells: Vec::new(),
            confidence: 0.93,
            source: TableSource::Model,
        };
        for (row, column, rowspan, colspan, text) in [
            (0, 0, 2, 1, "标签"),
            (0, 1, 1, 2, "line 1\nline 2"),
            (1, 1, 1, 1, "=12\t<&\""),
            (1, 2, 1, 1, "末"),
        ] {
            grid.cells.push(TableCell {
                row,
                column,
                rowspan,
                colspan,
                text: text.into(),
                rect: grid.cell_rect(row, column, rowspan, colspan).unwrap(),
                confidence: 0.9,
            });
        }
        assert!(grid.validate());
        let text = "标签\r\nline 1\nline 2\n=12\t<&\"\n末";
        let lines: Vec<_> = text
            .lines()
            .map(|text| crate::recognition::RecognizedLine {
                text: text.into(),
                rect: None,
                horizontal_direction: Default::default(),
                geometry: None,
                graphemes: None,
            })
            .collect();
        let region = Some((0..lines.len()).collect());
        TableRecognition {
            recognized: RecognizedText {
                text: text.into(),
                regions: Vec::new(),
                words: Vec::new(),
                lines,
            },
            grid: Some(grid),
            region,
            warning: None,
        }
    }

    #[test]
    fn table_json_preserves_multiline_cells_spans_and_original_ocr_text() {
        let result = result();
        let value = table_json(&result, OcrProfile::Small).unwrap();
        let value: serde_json::Value = serde_json::from_str(&value.to_string()).unwrap();
        assert_eq!(value["kind"], "table");
        assert_eq!(value["model"], "slanet-plus");
        assert_eq!(value["ocr_model"], "ppocrv6-small");
        assert_eq!(value["profile"], "small");
        assert_eq!(value["source"], "model");
        assert_eq!(value["rows"], 2);
        assert_eq!(value["columns"], 3);
        assert_eq!(value["source_lines"], 5);
        assert_eq!(value["region"], serde_json::json!([0, 1, 2, 3, 4]));
        assert_eq!(value["text"], result.recognized.text);
        assert_eq!(value["cells"][0]["rowspan"], 2);
        assert_eq!(value["cells"][0]["colspan"], 1);
        assert_eq!(value["cells"][1]["colspan"], 2);
        assert_eq!(value["cells"][1]["text"], "line 1\nline 2");
        assert_eq!(
            value["cells"][1]["rect"],
            rect_json(Rect {
                x: 40,
                y: 0,
                width: 120,
                height: 20
            })
        );
        assert!(value["tsv"]
            .as_str()
            .unwrap()
            .contains("\"line 1\nline 2\""));
        let html = value["html"].as_str().unwrap();
        assert!(html.contains("rowspan=\"2\""));
        assert!(html.contains("colspan=\"2\""));
        assert!(html.contains("line 1<br>line 2"));
        assert!(html.contains("&lt;&amp;&quot;"));
    }

    #[test]
    fn table_json_reports_fallback_source_and_keeps_text_when_structure_is_missing() {
        let mut result = result();
        result.grid.as_mut().unwrap().source = TableSource::Alignment;
        result.warning = Some("结构模型不可用".into());
        let fallback = table_json(&result, OcrProfile::Tiny).unwrap();
        assert_eq!(fallback["source"], "alignment");
        assert_eq!(fallback["warning"], "结构模型不可用");
        result.grid = None;
        let missing = table_json(&result, OcrProfile::Tiny).unwrap();
        assert_eq!(missing["text"], result.recognized.text);
        assert_eq!(missing["warning"], "结构模型不可用");
        for field in [
            "rows",
            "columns",
            "source",
            "confidence",
            "cells",
            "tsv",
            "html",
        ] {
            assert!(missing[field].is_null(), "{field}");
        }
    }
}
