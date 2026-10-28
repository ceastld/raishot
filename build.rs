fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTSHOT_EAGER_GPU_DLLS");
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
