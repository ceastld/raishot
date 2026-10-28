# RaiShot

[中文说明](README.zh-CN.md) · [Changelog](CHANGELOG.md) · [Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [Third-party notices](THIRD_PARTY_NOTICES.md)

RaiShot is a Windows screenshot, annotation, image pinning, OCR, scrolling capture, and screen recording tool written in Rust with GPUI. It runs independently, using native Windows APIs for capture and recording.

## Features

- Capture a region across multiple monitors, annotate it, and copy or save the result.
- Edit vector shapes and text with undo and redo; apply mosaic, blur, and transparent selection edges.
- Pin images or text above other windows, zoom them, and restore recently closed pins.
- Recognize text locally and decode QR codes.
- Capture scrolling content and record MP4 or GIF with system or microphone audio.
- Follow the Windows theme and choose Chinese or English in settings.

See [the Chinese documentation](README.zh-CN.md) for controls and current limitations.

## Run

Keep `raishot.exe`, `raishot-gpui.exe`, and `raishot-cli.exe` together, with their original names. Run `raishot.exe` to start the tray application. New configurations use **Ctrl+1** for capture and **Ctrl+2** to pin clipboard images or text; both shortcuts can be changed in settings.

The current target is Windows 10/11 x64. On HDR displays, captures are converted back to sRGB using the display's SDR content brightness (HDR highlights clip to white). Mixed-DPI behavior still needs further validation. OCR dependencies are downloaded on demand and checked against pinned sizes and SHA-256 values. Settings, optional history, recordings, and OCR cache live under `%LOCALAPPDATA%/RaiShot`.

## Build and contribute

Use Rust 1.90 or newer with the MSVC toolchain, Visual Studio C++ build tools, and a Windows SDK. GPUI is pinned in `vendor/gpui` with documented local patches; building and testing need nothing outside this repository.

```powershell
cargo check --locked
cargo build --locked
```

The internal code name `rustshot` (crate and module names, `RUSTSHOT_*` environment variables, window classes) stays fixed; the product name and executable names are defined only in `src/brand.rs`, the `[[bin]]` entries of `Cargo.toml`, `build.rs`, and `Tools/package.ps1`.

Start with the [development documentation](docs/README.md) for architecture, terminology, current work, technical decisions, and design prototypes. This context is included in the repository. See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution terms.

## License

RaiShot's own code is open source under the [MIT License](LICENSE). Commercial use, modification, redistribution, sublicensing, and private derivatives are permitted; preserve the copyright and license notice in copies or substantial portions of the software. See [the license guide](docs/license.md) for scope. Rights already lawfully granted for earlier releases are unaffected. Third-party code, model weights, runtime libraries, fonts, and other assets retain their own terms; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
