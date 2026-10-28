[CmdletBinding()]
param(
    [string]$TargetDirectory = 'target',
    [ValidateRange(1, 64)][int]$Jobs = 1,
    [switch]$Timings
)
$ErrorActionPreference = 'Stop'
$taskCompilerStack = [Environment]::GetEnvironmentVariable('RUST_MIN_STACK', 'Process')
# GPUI 组合视图的编译需要更大的 rustc 工作线程栈，仅作用于构建，不改变程序运行栈。
if ([string]::IsNullOrEmpty($taskCompilerStack)) { $env:RUST_MIN_STACK = '33554432' }
Push-Location -LiteralPath $PSScriptRoot
try {
    # 并发由调用方按内存配置；CI 输出 Cargo 报告，区分依赖编译与三个 EXE 的链接耗时。
    $taskCargoArguments = @('build', '--release', '--locked', '--bins', '--target-dir', $TargetDirectory, '--jobs', [string]$Jobs)
    if ($Timings) { $taskCargoArguments += '--timings' }
    & cargo @taskCargoArguments
    if ($LASTEXITCODE -ne 0) { throw "RustShot 构建失败：$LASTEXITCODE" }
    foreach ($taskName in @('raishot.exe','raishot-gpui.exe','raishot-cli.exe')) {
        Get-Item -LiteralPath (Join-Path $TargetDirectory "release\$taskName") | Select-Object FullName,Length
    }
} finally {
    Pop-Location
    [Environment]::SetEnvironmentVariable('RUST_MIN_STACK', $taskCompilerStack, 'Process')
}
