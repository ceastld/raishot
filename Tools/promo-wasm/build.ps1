# 把生产长截图拼接代码编成 WebAssembly，内嵌为 design/promo-stitch-wasm.js（file:// 也能加载）。
# 需要 rustup 的 wasm32-unknown-unknown 目标：rustup target add wasm32-unknown-unknown
$ErrorActionPreference = 'Stop'
$root = Resolve-Path "$PSScriptRoot/../.."
$targetDir = Join-Path $root 'target/promo-wasm'
$rustup = Get-Command rustup -ErrorAction Stop
$env:RUSTC = (& $rustup.Source which rustc).Trim()
$cargo = Join-Path (Split-Path $env:RUSTC) 'cargo.exe'

# 开启 wasm SIMD：行差与取最大值依赖向量化，标量 wasm 几乎拿不到优化收益。
$oldFlags = $env:RUSTFLAGS
$env:RUSTFLAGS = '-C target-feature=+simd128'
Push-Location $PSScriptRoot
try {
    & $cargo build --release --lib --target wasm32-unknown-unknown --target-dir $targetDir
    if ($LASTEXITCODE -ne 0) { throw "wasm 编译失败" }
} finally {
    Pop-Location
    $env:RUSTFLAGS = $oldFlags
}

$wasm = Join-Path $targetDir 'wasm32-unknown-unknown/release/rustshot_promo_wasm.wasm'
$bytes = [IO.File]::ReadAllBytes($wasm)
$b64 = [Convert]::ToBase64String($bytes)
$out = Join-Path $root 'design/promo-stitch-wasm.js'
$text = "// 由 Tools/promo-wasm/build.ps1 生成：src/stitch*.rs、seam_*.rs 的 WebAssembly 构建，勿手改。`n" +
    "window.RAISHOT_STITCH_WASM = '$b64';`n"
[IO.File]::WriteAllText($out, $text, [Text.UTF8Encoding]::new($false))

# 页面引用带内容哈希，网站更新后访客不会拿到缓存的旧引擎。
$hash = ([BitConverter]::ToString([Security.Cryptography.SHA256]::HashData($bytes)) -replace '-', '').Substring(0, 10).ToLower()
$page = Join-Path $root 'design/promo-long-capture.html'
$html = [IO.File]::ReadAllText($page)
$html = [regex]::Replace($html, 'promo-stitch-wasm\.js(\?v=[0-9a-f]+)?"', "promo-stitch-wasm.js?v=$hash`"")
[IO.File]::WriteAllText($page, $html, [Text.UTF8Encoding]::new($false))
"已写入 $out（wasm $($bytes.Length) 字节）"
