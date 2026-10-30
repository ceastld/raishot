#![cfg(windows)]

#[macro_use]
pub mod i18n;
#[macro_use]
pub mod brand;
mod annotation;
mod arrow_geometry;
mod audio_capture;
mod audio_devices;
mod autostart;
mod baidu_image_translate;
mod brush_cursor;
mod hand_cursor;
mod canvas_source;
mod capture;
mod capture_arrow_properties;
mod capture_canvas;
mod capture_color_dialog;
mod capture_color_pick;
mod capture_control_selection;
mod capture_cursor_nudge;
mod capture_double_click;
mod capture_failure;
mod capture_help;
mod capture_interaction;
mod capture_lock;
mod capture_mask;
mod capture_monitor_snap;
mod capture_ocr_overlay;
mod capture_ocr_result_click;
mod capture_pin_output;
#[cfg(test)]
mod capture_pin_output_tests;
mod capture_properties;
mod capture_property_popups;
mod capture_qr;
mod capture_refresh;
mod capture_selection_bar;
mod capture_size_input;
mod capture_standby;
mod capture_text_properties;
mod capture_text_selection;
mod capture_toolbar;
mod capture_view;
mod capture_watermark_input;
pub mod cli;
mod config_json;
mod config_recovery;
mod control_feedback;
mod control_probe_worker;
mod control_selection;
mod control_selection_smoke;
mod crop_expand_smoke;
#[cfg(test)]
mod crop_expand_tests;
mod crop_mask;
mod cursor;
mod customization_smoke;
mod desktop;
mod document;
mod editor;
#[cfg(test)]
mod editor_watermark_tests;
#[cfg(test)]
mod effect_parameter_tests;
mod export;
mod fast_capture;
mod fast_capture_smoke;
mod file_launch_command;
mod file_snapshot;
mod finish_parameter_control;
mod finish_widgets;
mod gdi_surface;
mod geometry;
mod gif_export;
mod gpui_capture;
mod gpui_smoke;
mod highlighter_smoke;
mod history;
mod host_shortcuts;
mod hotkey;
mod hotkey_registration;
mod hotkey_registration_smoke;
mod hotkey_status;
mod hybrid_text;
mod image_animation;
mod image_animation_playback;
mod image_apng;
mod image_apng_playback;
mod image_batch;
mod image_collage;
mod image_color_management;
mod image_color_profile;
mod image_color_transform;
mod image_collection;
mod image_container_metadata;
mod image_decode;
mod image_editor;
mod image_exif;
mod image_export_metadata;
mod image_format_samples;
mod image_ico;
mod image_metadata;
mod image_pdf;
mod image_resize;
mod image_preview;
mod image_quick_preview;
#[cfg(test)]
mod image_preview_scene_tests;
mod image_preview_smoke;
mod image_smoke;
mod image_split;
mod image_tiff;
mod image_tool_view;
mod image_tools;
mod image_transform;
mod image_translate;
mod image_viewer;
mod image_viewer_rotation;
mod image_webp_encode;
mod input_overlay;
mod interaction;
mod last_region;
mod launch_feedback;
mod library;
mod live_capture;
mod long_auto;
mod long_auto_native;
mod long_auto_resume;
mod long_auto_smoke;
mod long_browser_oracle;
mod long_browser_owner;
mod long_browser_smoke;
mod long_browser_stress_oracle;
mod long_browser_stress_smoke;
mod long_capture_border;
mod long_capture_pipeline;
mod long_capture_preview;
mod long_capture_runtime;
mod long_capture_smoke;
mod long_capture_view;
mod long_clip;
mod long_clip_canvas;
mod long_clip_chrome;
mod long_clip_export;
mod long_clip_input;
mod long_clip_navigation;
mod long_clip_parameters;
mod long_clip_preview;
mod long_clip_projection;
mod long_clip_ui;
mod long_document;
mod long_dxgi_memory_smoke;
mod long_editor;
mod long_editor_smoke;
mod long_frame_coalesce;
mod long_output;
mod long_pipeline_smoke;
mod long_pipeline_ui_smoke;
mod long_preview_background_smoke;
mod long_preview_stability_smoke;
mod long_preview_strip;
mod long_region;
mod long_region_input;
mod long_region_native_smoke;
mod long_runtime_memory_smoke;
mod long_smoke;
mod long_workflow_smoke;
mod mask_quality;
mod media_foundation;
mod media_playback_smoke;
mod media_playback_sync;
mod mesh_coverage;
mod ocr_markup;
mod ocr_panel;
mod ocr_projection;
mod ocr_result_filter;
mod ocr_result_selection;
mod ocr_sensitive;
mod ocr_sensitive_effect;
mod output_projection;
mod parameter_control;
mod parameter_smoke;
mod path_resource_smoke;
mod pencil;
mod pencil_continuity_smoke;
mod pencil_input_smoke;
mod pencil_quality;
mod pencil_samples;
mod pin_annotation;
mod pin_app;
mod pin_autosave;
mod pin_color_display;
mod pin_color_magnifier;
mod pin_color_sample;
mod pin_detail_smoke;
mod pin_drag;
mod pin_drag_request;
mod pin_drop;
mod pin_export;
mod pin_export_detail_smoke;
mod pin_export_smoke;
mod pin_geometry;
mod pin_handoff_smoke;
mod pin_interaction;
mod pin_interaction_smoke;
mod pin_keys;
mod pin_menu;
mod pin_menu_smoke;
mod pin_model;
mod qr_smoke;
pub mod quick_action;
mod quick_action_flow;
mod quick_action_store;
#[cfg(test)]
mod quick_action_tests;
mod quick_toast;
mod settings_quick_actions;
mod table_clipboard;
#[cfg(test)]
mod table_eval;
mod table_export;
mod table_grid;
mod table_layout;
mod table_lines;
#[cfg(test)]
mod table_metrics;
// 实时窗口贴图纯模型；W3 接入贴图窗前只被测试使用。
#[cfg_attr(not(test), allow(dead_code))]
mod pin_live_dwm;
#[cfg_attr(not(test), allow(dead_code))]
mod pin_live_model;
mod pin_live_smoke;
mod pin_live_snapshot;
mod pin_live_view;
mod pin_native;
mod pin_presentation;
mod pin_presentation_host;
mod pin_presentation_smoke;
mod pin_preview_lifecycle;
mod pin_properties;
mod pin_property_bar;
mod pin_raster_document;
mod pin_refill;
pub(crate) mod pin_shadow;
mod pin_smoke;
mod pin_store;
mod pin_text_document;
mod pin_text_selection;
mod pin_text_source;
mod pin_title;
mod pin_tool_picker;
mod pin_toolbar;
mod pin_toolbar_layout;
mod pin_toolbar_smoke;
mod pin_view;
mod pin_workspace;
mod pin_workspace_runtime;
mod pin_zoom;
mod pin_zoom_preferences_smoke;
mod pin_zoom_smoke;
mod ppocr;
mod preferences;
mod preview_audio;
mod preview_audio_clock;
mod preview_audio_device_clock;
mod preview_middle_pin;
mod process_memory;
mod projected_scene;
mod projected_text;
mod property_items;
mod radius_bulk_smoke;
mod radius_handle;
#[cfg(test)]
mod radius_handle_edit_tests;
mod radius_handle_smoke;
mod raster;
mod recognition;
mod recording;
mod recording_codec_benchmark;
mod recording_color_check;
mod recording_encoder_caps;
mod recording_encoder_policy;
mod recording_encoder_trial;
#[cfg(test)]
mod recording_fault_stream;
mod recording_gpu;
mod recording_key_owner;
mod recording_library_metadata;
mod recording_library_model;
mod recording_options;
mod recording_preview;
#[cfg(test)]
mod recording_quality_lab;
mod recording_size_layout;
mod recording_view;
mod remote_display;
mod rotation_smoke;
mod save_options;
mod save_panel;
mod save_smoke;
mod save_store;
mod save_workflow;
mod scene;
mod seam_alignment;
mod selection_action_layout;
mod selection_assist;
mod selection_chrome;
mod selection_effect_smoke;
mod selection_finish;
mod selection_finish_smoke;
mod selection_finish_store;
mod selection_finish_view;
#[cfg(test)]
mod selection_parameter_tests;
mod selection_transform;
mod selection_wheel;
mod settings_autostart;
mod settings_hotkey;
mod settings_hotkey_native;
mod settings_pin;
mod settings_save;
mod settings_toolbar;
mod settings_trigger;
mod settings_view;
mod settings_widgets;
#[cfg(test)]
mod smart_effect_projection_tests;
mod smart_effect_scene;
pub mod smart_mosaic;
mod spotlight;
#[cfg(test)]
mod spotlight_edit_tests;
mod spotlight_smoke;
mod startup_bitmap;
mod startup_bounds;
mod startup_handoff;
mod startup_output;
mod startup_preview;
mod startup_probe;
mod startup_surface;
mod storage_db;
mod startup_surface_input;
mod startup_surface_smoke;
mod stitch;
mod stitch_content;
mod text_contract;
mod text_contract_smoke;
#[cfg(test)]
mod text_contract_tests;
mod text_field;
mod text_format;
mod text_geometry;
mod text_input;
mod text_layout;
mod text_paint;
mod text_preview;
mod theme;
mod tool_presets;
mod tool_recent_captures;
mod tool_registry;
mod tool_window_chrome;
mod toolbar_above_smoke;
mod toolbar_config;
mod toolbar_drag;
mod toolbar_groups;
mod toolbar_icons;
mod toolbar_layout;
mod toolbar_smoke;
mod toolbar_widgets;
mod transform;
mod translate;
mod translation_protection;
mod api_secret;
mod formula;
mod formula_layout;
mod markdown_document;
mod markdown_view;
mod markdown_export;
mod uia_log;
mod vector_aa_smoke;
mod vector_mesh;
mod vector_paint;
mod vector_path;
mod vector_quality;
mod viewport;
mod video_controls;
mod video_file_queue;
mod video_frame_export;
mod video_shortcuts;
mod video_view;
mod watermark_edit;
#[cfg(test)]
mod watermark_edit_tests;
mod watermark_input;
mod watermark_long_memory_smoke;
mod watermark_preview;
#[cfg(test)]
mod watermark_scene_tests;
mod watermark_toolbar_smoke;
mod wav_track;

mod tray;
mod tray_popup;
mod tray_request;
mod trigger;
mod trigger_listener;
mod ui;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn finish_launch_feedback() {
    launch_feedback::finish();
}

pub fn install_worker_diagnostics() {
    capture_failure::install_panic_hook();
}

pub fn run_gui() {
    if let Err(error) = ui::run() {
        report_error(&error.to_string());
    }
}

pub fn run_gpui(full_screen: bool, all_screens: bool, smoke: bool) -> Result<()> {
    let result = gpui_capture::run(full_screen, all_screens, smoke);
    if !smoke {
        if let Err(error) = result.as_ref() {
            if !capture_standby::is_cancelled(error.as_ref()) {
                capture_failure::record("gpui_capture", &error.to_string());
            }
        }
    }
    result
}

pub fn run_gpui_quick(full_screen: bool, all_screens: bool, smoke: bool, quick: u32) -> Result<()> {
    let result = gpui_capture::run_quick(full_screen, all_screens, smoke, quick);
    if !smoke {
        if let Err(error) = result.as_ref() {
            if !capture_standby::is_cancelled(error.as_ref()) {
                capture_failure::record("gpui_capture", &error.to_string());
            }
        }
    }
    result
}

/// 托盘弹层待命进程：等托盘请求后才初始化 GPUI。
pub fn run_tray_popup_standby(args: &[String]) -> Result<()> {
    tray_popup::run_standby(args)
}

/// 托盘弹层冷启动拆分：单独测 DXGI 工厂与 D3D11 设备创建。
pub fn run_tray_popup_d3d_probe(args: &[String]) -> Result<()> {
    tray_popup::run_d3d_probe(args)
}

/// 托盘弹层测速：本进程扮演托盘，逐次请求并打印各阶段毫秒数。
pub fn run_tray_popup_bench(args: &[String]) -> Result<()> {
    let number = |flag: &str, default: u64| {
        args.iter()
            .position(|arg| arg == flag)
            .and_then(|index| args.get(index + 1))
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    tray_popup::run_bench(
        number("--runs", 10) as usize,
        std::time::Duration::from_millis(number("--settle-ms", 1500)),
        args.iter().any(|arg| arg == "--prewarm"),
    )
}

pub fn run_capture_standby(args: &[String]) -> Result<()> {
    let result: Result<()> = (|| {
        if let Some(request) = capture_standby::wait(args)? {
            let capture_result = gpui_capture::run_quick(
                request.full_screen,
                request.all_screens,
                false,
                request.quick,
            );
            // 先记录真实失败，再 join 原 controller；cleanup 阻塞也不能吞掉失败原因。
            if let Err(error) = capture_result.as_ref() {
                if !capture_standby::is_cancelled(error.as_ref()) {
                    capture_failure::record("standby_capture", &error.to_string());
                }
            }
            // Controller 正常 join/清理错误优先保留；只有明确的 owner 取消自然结束。
            request.finish()?;
            match capture_result {
                Err(error) if capture_standby::is_cancelled(error.as_ref()) => {}
                result => result?,
            }
        }
        Ok(())
    })();
    if let Err(error) = result.as_ref() {
        if !capture_standby::is_cancelled(error.as_ref()) {
            capture_failure::record("standby_session", &error.to_string());
        }
    }
    // 互斥锁拒绝或首屏被禁用时仍需回收尚未消费的预热窗口。
    startup_surface::discard_standby_warm();
    fast_capture::discard_prewarmed_thread();
    result
}

pub fn trace_worker_entry() {
    startup_probe::trace_worker_entry();
}

pub fn run_startup_probe() -> Result<()> {
    let started = std::time::Instant::now();
    let result = gpui_capture::run_startup_probe();
    let recycled = startup_probe::recycled(started, result.is_ok());
    result.and(recycled)
}

pub fn run_fast_capture_smoke() -> Result<()> {
    fast_capture_smoke::run()
}
pub fn run_spotlight_smoke() -> Result<()> {
    gpui_capture::run_spotlight_smoke()
}
pub fn run_radius_handle_smoke() -> Result<()> {
    gpui_capture::run_radius_handle_smoke()
}
pub fn run_parameter_smoke() -> Result<()> {
    gpui_capture::run_parameter_smoke()
}

pub fn run_long_capture_smoke() -> Result<()> {
    long_capture_smoke::run()
}
pub fn run_long_workflow_smoke() -> Result<()> {
    long_workflow_smoke::run()
}
pub fn run_long_editor_smoke() -> Result<()> {
    long_editor_smoke::run()
}
pub fn run_long_clip_preview(path: &std::path::Path) -> Result<()> {
    long_editor_smoke::run_preview(path)
}
pub fn run_long_memory_smoke() -> Result<()> {
    long_editor_smoke::run_memory()
}
pub fn run_long_preview_stability_smoke() -> Result<()> {
    long_preview_stability_smoke::run()
}
pub fn run_long_preview_background_smoke() -> Result<()> {
    long_preview_background_smoke::run()
}
pub fn run_long_runtime_memory_smoke() -> Result<()> {
    long_runtime_memory_smoke::run()
}
pub fn run_pin_workspace_overlap_smoke() -> Result<()> {
    long_runtime_memory_smoke::run_workspace_overlap()
}
pub fn run_pin_workspace_idle_smoke() -> Result<()> {
    long_runtime_memory_smoke::run_workspace_idle()
}
pub fn run_pin_presentation_smoke() -> Result<()> {
    pin_presentation_smoke::run()
}
pub fn run_long_dxgi_memory_smoke() -> Result<()> {
    long_dxgi_memory_smoke::run()
}
pub fn run_long_pipeline_smoke() -> Result<()> {
    long_pipeline_smoke::run()
}
pub fn run_long_auto_smoke() -> Result<()> {
    long_auto_smoke::run()
}
pub fn run_long_region_smoke() -> Result<()> {
    long_region_native_smoke::run()
}
pub fn run_long_browser_smoke() -> Result<()> {
    long_browser_smoke::run()
}

pub fn run_long_browser_stress_smoke() -> Result<()> {
    long_browser_stress_smoke::run()
}

pub fn run_long_browser_ui_auto_smoke() -> Result<()> {
    long_browser_stress_smoke::ui_auto::run()
}

pub fn run_long_browser_nested_smoke() -> Result<()> {
    long_browser_stress_smoke::nested::run()
}

pub fn run_long_browser_native_stress_smoke() -> Result<()> {
    long_browser_stress_smoke::native_stress::run()
}

pub fn run_path_resource_smoke() -> Result<()> {
    path_resource_smoke::run()
}
pub fn run_startup_surface_smoke() -> Result<()> {
    gpui_capture::run_startup_surface_smoke()
}

pub fn run_selection_finish_smoke() -> Result<()> {
    gpui_capture::run_selection_finish_smoke()
}

pub fn run_customization_smoke() -> Result<()> {
    gpui_capture::run_customization_smoke()
}

pub fn run_save_smoke() -> Result<()> {
    save_smoke::run()
}

pub fn report_error(message: &str) {
    // GUI 启动失败仍给出可见错误；没有常驻日志线程。
    let text: Vec<u16> = format!("{}：{message}", brand::NAME)
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
            None,
            windows::core::PCWSTR(text.as_ptr()),
            &windows::core::HSTRING::from(brand::NAME),
            windows::Win32::UI::WindowsAndMessaging::MB_OK
                | windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
        );
    }
}

/// `--settings` 打开主界面（`--page pins|history|settings` 指定页），`--history` 打开截图历史。
pub fn run_auxiliary(settings: bool, args: &[String]) -> Result<()> {
    use settings_view::main_page::MainPage;
    // `--history` 是旧入口，现打开主界面历史页。
    let page = MainPage::from_args(args).or((!settings).then_some(MainPage::History));
    // 常见的重复打开在初始化 GPUI/GPU 之前完成；创建时还会加锁复查。
    if settings_view::SettingsView::activate_existing(page)? {
        return Ok(());
    }
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
    use gpui::{App, Application};
    let errors = std::rc::Rc::new(std::cell::RefCell::new(None));
    let final_error = errors.clone();
    Application::new()
        .with_assets(crate::toolbar_icons::IconAssets)
        .run(move |cx: &mut App| {
            crate::theme::install(cx);
            let result = settings_view::SettingsView::open_page(cx, page);
            if let Err(e) = result {
                *errors.borrow_mut() = Some(e.to_string());
                cx.quit();
                return;
            }
            // 设置请求已交给其它进程的窗口时，本进程没有窗口，直接结束。
            if cx.windows().is_empty() {
                cx.quit();
                return;
            }
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty()
                    && !crate::pin_workspace_runtime::is_restoring(cx)
                    && !crate::pin_live_view::has_open_windows()
                {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
    let error = final_error.borrow_mut().take();
    if let Some(e) = error {
        Err(e.into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod vector_tests;

pub mod local_fill;
pub(crate) mod local_fill_scene;
#[cfg(test)]
mod local_fill_scene_tests;
mod local_fill_smoke;
#[cfg(test)]
mod local_fill_tests;

#[cfg(test)]
mod highlighter_tests;

#[cfg(test)]
mod transform_tests;

#[cfg(test)]
mod text_format_tests;
#[cfg(test)]
mod text_parameter_edit_tests;

#[cfg(test)]
mod drawing_detail_tests;

#[cfg(test)]
mod tool_preset_integration_tests;

#[cfg(test)]
mod radius_bulk_tests;

#[cfg(test)]
mod filled_rectangle_tests;

#[cfg(test)]
mod mask_tests;

pub fn run_image_editor(path: Option<std::path::PathBuf>, smoke: bool) -> Result<()> {
    image_editor::run(path, smoke)
}

pub fn run_recording_ui_smoke() -> Result<()> {
    recording_view::smoke()
}
pub fn run_image_preview_smoke() -> Result<()> {
    image_preview_smoke::run()
}
pub fn run_qr_smoke() -> Result<()> {
    qr_smoke::run()
}
pub fn run_watermark_toolbar_smoke() -> Result<()> {
    watermark_toolbar_smoke::run()
}

pub fn run_watermark_long_memory_smoke() -> Result<()> {
    watermark_long_memory_smoke::run()
}

pub fn run_vector_aa_smoke() -> Result<()> {
    gpui_capture::run_aa_smoke()
}

pub fn run_rotation_smoke() -> Result<()> {
    gpui_capture::run_rotation_smoke()
}

pub fn run_pin(mode: &str) -> Result<()> {
    pin_app::run(mode)
}
pub fn run_pin_record(id: &str) -> Result<()> {
    pin_app::run_record(id.to_owned())
}

pub fn run_file_launch(args: &[String]) -> Result<bool> {
    file_launch_command::execute_worker(args)
}

#[cfg(test)]
mod pin_draw_bench;
