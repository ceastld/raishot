<div align="center">
  <img src="assets/app.svg" width="88" height="88" alt="RaiShot icon">
  <h1>RaiShot</h1>
  <p><strong>Screenshots, scrolling capture, pinning, and recording for Windows.</strong></p>
  <p>
    <a href="https://github.com/ceastld/raishot/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/ceastld/raishot/ci.yml?branch=main&amp;label=CI" alt="Windows CI"></a>
    <a href="https://github.com/ceastld/raishot/releases/latest"><img src="https://img.shields.io/github/v/release/ceastld/raishot?color=078267" alt="Latest release"></a>
    <a href="https://github.com/ceastld/raishot/releases/latest"><img src="https://img.shields.io/badge/Windows-10%20%2F%2011%20x64-0078D4" alt="Windows 10 and 11 x64"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-078267" alt="MIT License"></a>
  </p>
  <p><strong><a href="https://github.com/ceastld/raishot/releases/latest">Download for Windows</a></strong> · <a href="README.zh-CN.md">简体中文</a> · <a href="CHANGELOG.md">Changelog</a> · <a href="docs/README.md">Documentation</a></p>
</div>

RaiShot helps you capture a screen region, mark what matters, and copy or save the result. Pin an image or a note beside your work, stitch a scrolling page, extract text locally, or record a short walkthrough—all from the system tray.

Built with **Rust and GPUI**, with native Windows capture and recording. The interface supports **English and Simplified Chinese**, with light and dark themes that follow Windows by default.

## Features

| Feature | What you can do |
| --- | --- |
| **Screenshot** | Capture a region, a window, or multiple monitors. Adjust the selection before copying or saving. |
| **Annotation** | Add arrows, shapes, text, highlights, mosaic, and blur. Select and edit annotations, with undo and redo. |
| **Scrolling capture** | Stitch vertical or horizontal content with manual or automatic scrolling, including scrolling back over captured content. |
| **Pin to screen** | Keep images and text above other windows. Zoom, annotate, adjust opacity, enable click-through, and restore recently closed pins. |
| **OCR & QR codes** | Recognize text locally, copy the result, and read or generate QR codes. |
| **Screen recording** | Record MP4 with system or microphone audio, or export GIFs. Show mouse clicks and keystrokes in recordings. |

The **library** brings screenshots, pins, and recordings together, with image and video previews and tools for comparing, combining, and batch exporting images.

### Long pages, one image

Capture a webpage, a conversation, or a wide document beyond the visible area. Scroll manually or let RaiShot scroll automatically; the stitched preview updates as you go. You can reverse direction and revisit captured content without simply appending it again.

The stitcher accounts for fixed headers and floating elements when aligning frames and choosing seams. See the [scrolling capture notes](docs/references/long-capture-floating-elements-2026-10-26.md) for the design and current verification boundaries.

## Get started

**Requires Windows 10 or 11, x64.**

1. Open [the latest release](https://github.com/ceastld/raishot/releases/latest), download the `RaiShot-*-setup-x64.exe` installer and run it. It installs for the current user without administrator rights and starts the tray when done.
2. For a portable copy, download `RaiShot.*.zip`, extract the whole archive to a folder, and run **`raishot.exe`**. Keep the included EXEs and DLLs together.
3. Press **Ctrl+1** to capture, or **Ctrl+2** to pin an image or text from the clipboard. Both shortcuts can be changed in settings.

Click the tray icon to capture; right-click it for the library, settings, and other tools. On Windows N editions, screen recording requires the Media Feature Pack.

### A few useful shortcuts

| Context | Shortcut | Action |
| --- | --- | --- |
| Global | **Ctrl+1** | Start a screenshot |
| Global | **Ctrl+2** | Pin clipboard image or text |
| Capture selection | **L** / **G** | Start scrolling capture / screen recording |
| Capture selection | **R** | Recognize text |
| Capture selection | **Enter** / **Ctrl+S** | Copy and finish / save |
| Capture | **Esc** | Close the current panel, cancel an operation, or exit capture |

The global shortcuts above are defaults for a new configuration. The other keys apply to a normal capture selection; text editing, OCR, and other active tools take priority. See the [full controls and behavior guide](docs/behavior.md) for details.

### Local data and OCR

Settings, the pin workspace, optional capture history, recordings, and the OCR cache are stored under `%LOCALAPPDATA%/RaiShot`. Capture history can be enabled in settings.

Text recognition runs locally. On first use, RaiShot downloads the OCR resources it needs and verifies their pinned sizes and SHA-256 checksums. Online translation is a separate, optional feature that uses the service you configure.

## Build and contribute

Build on Windows with **Rust 1.90+ (MSVC)**, **Visual Studio C++ Build Tools**, and a **Windows SDK**:

```powershell
git clone https://github.com/ceastld/raishot.git
cd raishot
cargo build --locked
```

GPUI is pinned in the repository with [documented local patches](vendor/README.md). Build and test instructions, architecture, technical decisions, and current work are available in the [development documentation](docs/README.md).

Bug reports, reproducible test cases, translations, and code contributions are welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md); report bugs and request features in [Issues](https://github.com/ceastld/raishot/issues). For security-sensitive reports, follow [SECURITY.md](SECURITY.md).

## Documentation

- [Usage and shortcuts](docs/behavior.md)
- [Changelog](CHANGELOG.md) · [Current status and known limitations](docs/status.md) · [Roadmap](docs/roadmap.md)
- [Build instructions](docs/development.md) · [Release process](docs/releasing.md) · [Architecture](docs/architecture.md) · [Technical notes](docs/references/README.md)

The README describes the main workflows. Release-specific changes are listed in the changelog; the current source also includes work in progress documented in the status page.

## License

RaiShot is released under the **[MIT License](LICENSE)**. Third-party dependencies, models, fonts, and other assets retain their own licenses; see [Third-party notices](THIRD_PARTY_NOTICES.md) and the [license guide](docs/license.md).
