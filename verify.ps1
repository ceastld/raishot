[CmdletBinding()]
param([switch]$Gpu, [switch]$Recognition, [switch]$Recording, [string]$TargetDirectory = 'target')

$ErrorActionPreference = 'Stop'
$taskCompilerStack = [Environment]::GetEnvironmentVariable('RUST_MIN_STACK', 'Process')
if ([string]::IsNullOrEmpty($taskCompilerStack)) { $env:RUST_MIN_STACK = '33554432' }
# 界面冒烟按中文文案断言；未显式指定时固定中文，不受本机语言设置影响。
if ([string]::IsNullOrEmpty($env:RUSTSHOT_LANGUAGE)) { $env:RUSTSHOT_LANGUAGE = 'zh-CN' }
Push-Location -LiteralPath $PSScriptRoot
try {
    & cargo fmt --all -- --check
    if ($LASTEXITCODE -ne 0) { throw 'Rust 格式检查失败' }
    & cargo clippy --all-targets --locked --target-dir $TargetDirectory -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'Rust 静态检查失败' }
    & cargo test --lib --locked --target-dir $TargetDirectory
    if ($LASTEXITCODE -ne 0) { throw '核心测试失败' }
    & cargo build --release --locked --target-dir $TargetDirectory
    if ($LASTEXITCODE -ne 0) { throw 'Release 构建失败' }
    [Environment]::SetEnvironmentVariable('RUST_MIN_STACK', $taskCompilerStack, 'Process')
    if ($Gpu) {
        $taskCli = Join-Path $TargetDirectory 'release\raishot-cli.exe'
        foreach ($taskMode in @('--self-test', '--ui-smoke', '--long-self-test', '--resident-smoke')) {
            & $taskCli $taskMode
            if ($LASTEXITCODE -ne 0) { throw "GPU / 界面验证失败：$taskMode" }
        }
    }
    $taskCli = Join-Path $TargetDirectory 'release\raishot-cli.exe'
    if ($Recognition) {
        & $taskCli --ocr-self-test
        if ($LASTEXITCODE -ne 0) { throw 'Windows OCR 验证失败（需要本机语言包）' }
    }
    if ($Recording) {
        & $taskCli --record-self-test
        if ($LASTEXITCODE -ne 0) { throw '录屏验证失败（系统 Media Foundation 编码）' }
    }
    Write-Output 'RustShot 验证通过。'
} finally {
    Pop-Location
    [Environment]::SetEnvironmentVariable('RUST_MIN_STACK', $taskCompilerStack, 'Process')
}
