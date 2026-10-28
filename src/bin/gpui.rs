#![windows_subsystem = "windows"]

fn main() {
    rustshot::install_worker_diagnostics();
    rustshot::finish_launch_feedback();
    rustshot::trace_worker_entry();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--capture-standby") {
        if let Err(error) = rustshot::run_capture_standby(&args) {
            eprintln!("{} 快速截图进程：{error}", rustshot::brand::NAME);
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|arg| arg == "--tray-popup-standby") {
        if let Err(error) = rustshot::run_tray_popup_standby(&args) {
            eprintln!("{} 托盘弹层：{error}", rustshot::brand::NAME);
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|arg| arg == "--tray-popup-d3d-probe") {
        if let Err(error) = rustshot::run_tray_popup_d3d_probe(&args) {
            eprintln!("{} D3D 探测：{error}", rustshot::brand::NAME);
            std::process::exit(1);
        }
        return;
    }
    if args.first().is_some_and(|arg| arg == "--tray-popup-bench") {
        if let Err(error) = rustshot::run_tray_popup_bench(&args) {
            eprintln!("{} 托盘弹层测速：{error}", rustshot::brand::NAME);
            std::process::exit(1);
        }
        return;
    }
    let smoke = args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--smoke"
                | "--vector-aa-smoke"
                | "--spotlight-smoke"
                | "--radius-handle-smoke"
                | "--parameter-smoke"
                | "--rotation-smoke"
                | "--selection-finish-smoke"
                | "--customization-smoke"
                | "--save-smoke"
                | "--pin-smoke"
                | "--pin-workspace-overlap-smoke"
                | "--pin-workspace-idle-smoke"
                | "--pin-presentation-smoke"
                | "--startup-probe"
                | "--fast-capture-smoke"
                | "--path-resource-smoke"
                | "--startup-surface-smoke"
                | "--long-capture-smoke"
                | "--long-workflow-smoke"
                | "--long-editor-smoke"
                | "--long-memory-smoke"
                | "--long-preview-stability-smoke"
                | "--long-preview-background-smoke"
                | "--long-runtime-memory-smoke"
                | "--long-dxgi-memory-smoke"
                | "--long-pipeline-smoke"
                | "--long-auto-smoke"
                | "--long-region-smoke"
                | "--long-browser-smoke"
                | "--long-browser-stress-smoke"
                | "--long-browser-ui-auto-smoke"
                | "--long-browser-nested-smoke"
                | "--long-browser-native-stress-smoke"
                | "--image-preview-smoke"
                | "--qr-smoke"
                | "--recording-ui-smoke"
                | "--watermark-toolbar-smoke"
                | "--watermark-long-memory-smoke"
        )
    });
    let full_screen = args.iter().any(|arg| arg == "--fullscreen");
    let all_screens = args.iter().any(|arg| arg == "--all-screens");
    let quick = match args.iter().position(|arg| arg == "--quick") {
        Some(index) => args
            .get(index + 1)
            .ok_or("--quick 缺少动作编码")
            .and_then(|value| {
                rustshot::quick_action::QuickRecipe::parse_code(value)
                    .map_err(|_| "--quick 动作编码无效")
            })
            .and_then(|recipe| match recipe {
                Some(recipe) => recipe.pack().map_err(|_| "--quick 动作编码无效"),
                None => Ok(0),
            }),
        None => Ok(0),
    };
    let quick = match quick {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{} GPUI：{error}", rustshot::brand::NAME);
            if !smoke {
                rustshot::report_error(error);
            }
            std::process::exit(1);
        }
    };
    let file_launch = rustshot::run_file_launch(&args);
    let result = if !matches!(file_launch.as_ref(), Ok(false)) {
        file_launch.map(|_| ())
    } else if let Some(index) = args.iter().position(|a| a == "--long-clip-preview") {
        rustshot::run_long_clip_preview(std::path::Path::new(
            args.get(index + 1).map_or("", String::as_str),
        ))
    } else if let Some(index) = args.iter().position(|a| a == "--pin-record") {
        rustshot::run_pin_record(args.get(index + 1).map_or("", String::as_str))
    } else if let Some(mode) = args.iter().find(|a| {
        matches!(
            a.as_str(),
            "--pin-restore"
                | "--pin-file"
                | "--pin-clipboard"
                | "--pin-smoke"
                | "--pin-workspace"
                | "--pin-session-restore"
        )
    }) {
        rustshot::run_pin(mode)
    } else if args.iter().any(|a| a == "--pin-workspace-overlap-smoke") {
        rustshot::run_pin_workspace_overlap_smoke()
    } else if args.iter().any(|a| a == "--pin-workspace-idle-smoke") {
        rustshot::run_pin_workspace_idle_smoke()
    } else if args.iter().any(|a| a == "--pin-presentation-smoke") {
        rustshot::run_pin_presentation_smoke()
    } else if args.iter().any(|a| a == "--watermark-toolbar-smoke") {
        rustshot::run_watermark_toolbar_smoke()
    } else if args.iter().any(|a| a == "--watermark-long-memory-smoke") {
        rustshot::run_watermark_long_memory_smoke()
    } else if args.iter().any(|a| a == "--spotlight-smoke") {
        rustshot::run_spotlight_smoke()
    } else if args.iter().any(|a| a == "--radius-handle-smoke") {
        rustshot::run_radius_handle_smoke()
    } else if args.iter().any(|a| a == "--parameter-smoke") {
        rustshot::run_parameter_smoke()
    } else if args.iter().any(|a| a == "--fast-capture-smoke") {
        rustshot::run_fast_capture_smoke()
    } else if args.iter().any(|a| a == "--long-capture-smoke") {
        rustshot::run_long_capture_smoke()
    } else if args.iter().any(|a| a == "--long-workflow-smoke") {
        rustshot::run_long_workflow_smoke()
    } else if args.iter().any(|a| a == "--long-editor-smoke") {
        rustshot::run_long_editor_smoke()
    } else if args.iter().any(|a| a == "--long-memory-smoke") {
        rustshot::run_long_memory_smoke()
    } else if args.iter().any(|a| a == "--long-preview-stability-smoke") {
        rustshot::run_long_preview_stability_smoke()
    } else if args.iter().any(|a| a == "--long-preview-background-smoke") {
        rustshot::run_long_preview_background_smoke()
    } else if args.iter().any(|a| a == "--long-runtime-memory-smoke") {
        rustshot::run_long_runtime_memory_smoke()
    } else if args.iter().any(|a| a == "--long-dxgi-memory-smoke") {
        rustshot::run_long_dxgi_memory_smoke()
    } else if args.iter().any(|a| a == "--long-pipeline-smoke") {
        rustshot::run_long_pipeline_smoke()
    } else if args.iter().any(|a| a == "--long-auto-smoke") {
        rustshot::run_long_auto_smoke()
    } else if args.iter().any(|a| a == "--long-region-smoke") {
        rustshot::run_long_region_smoke()
    } else if args.iter().any(|a| a == "--long-browser-smoke") {
        rustshot::run_long_browser_smoke()
    } else if args.iter().any(|a| a == "--long-browser-stress-smoke") {
        rustshot::run_long_browser_stress_smoke()
    } else if args.iter().any(|a| a == "--long-browser-ui-auto-smoke") {
        rustshot::run_long_browser_ui_auto_smoke()
    } else if args.iter().any(|a| a == "--long-browser-nested-smoke") {
        rustshot::run_long_browser_nested_smoke()
    } else if args
        .iter()
        .any(|a| a == "--long-browser-native-stress-smoke")
    {
        rustshot::run_long_browser_native_stress_smoke()
    } else if args.iter().any(|a| a == "--recording-ui-smoke") {
        rustshot::run_recording_ui_smoke()
    } else if args.iter().any(|a| a == "--image-preview-smoke") {
        rustshot::run_image_preview_smoke()
    } else if args.iter().any(|a| a == "--qr-smoke") {
        rustshot::run_qr_smoke()
    } else if args.iter().any(|a| a == "--path-resource-smoke") {
        rustshot::run_path_resource_smoke()
    } else if args.iter().any(|a| a == "--startup-surface-smoke") {
        rustshot::run_startup_surface_smoke()
    } else if args.iter().any(|a| a == "--startup-probe") {
        rustshot::run_startup_probe()
    } else if args.iter().any(|a| a == "--customization-smoke") {
        rustshot::run_customization_smoke()
    } else if args.iter().any(|a| a == "--save-smoke") {
        rustshot::run_save_smoke()
    } else if args.iter().any(|a| a == "--selection-finish-smoke") {
        rustshot::run_selection_finish_smoke()
    } else if args.iter().any(|a| a == "--vector-aa-smoke") {
        rustshot::run_vector_aa_smoke()
    } else if args.iter().any(|a| a == "--rotation-smoke") {
        rustshot::run_rotation_smoke()
    } else if args.iter().any(|a| a == "--settings" || a == "--history") {
        rustshot::run_auxiliary(args.iter().any(|a| a == "--settings"), &args)
    } else {
        rustshot::run_gpui_quick(full_screen, all_screens, smoke, quick)
    };
    if let Err(error) = result {
        eprintln!("{} GPUI：{error}", rustshot::brand::NAME);
        if !smoke {
            rustshot::report_error(&error.to_string());
        }
        std::process::exit(1);
    }
}
