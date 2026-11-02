fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-env-changed=RUSTSHOT_EAGER_GPU_DLLS");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // GPUI 从 EXE 的编号 1 图标资源设置窗口类；与托盘共用多尺寸 ICO。
        embed_resource::compile_for_everything("assets/app.rc", embed_resource::NONE)
            .manifest_required()
            .expect("无法嵌入应用图标资源");
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
        && std::env::var("RUSTSHOT_EAGER_GPU_DLLS").as_deref() != Ok("1")
    {
        // 仅延迟工作进程的函数导入：原生首屏不必等待 GPU / 字体 DLL 装载。
        // 程序名须与 Cargo.toml 的 [[bin]] 及 src/brand.rs 的 GPUI_EXE 一致。
        // 第一次实际调用仍由系统加载；不卸载设备 DLL，不改变 DXGI 回退路径。
        println!("cargo:rustc-link-arg-bin=raishot-gpui=delayimp.lib");
        for dll in ["d3d11.dll", "dxgi.dll", "dcomp.dll", "dwrite.dll"] {
            println!("cargo:rustc-link-arg-bin=raishot-gpui=/DELAYLOAD:{dll}");
        }
    }
}
