param(
    [string]$TargetDirectory = 'target/next',
    [ValidateSet('debug','release')][string]$Configuration = 'release'
)
$ErrorActionPreference = 'Stop'
$taskRoot = Split-Path -Parent $PSScriptRoot
$taskExe = Join-Path $taskRoot "$TargetDirectory/$Configuration/raishot-gpui.exe"
if (@(Get-CimInstance Win32_Process -Filter "Name='raishot-gpui.exe'" | Where-Object { $_.CommandLine -match '--pin-smoke' }).Count -ne 0) {
    throw '已有贴图专用验收进程，未重复启动。'
}
# 本专项只读自有 GPU 帧、向自有 HWND 投递事件，并使用独立配置与持久化目录。
# 普通截图/贴图可继续运行，不关闭或更新用户会话。
$taskDir = Join-Path $taskRoot ('target/qa/pin-enlarged-export-native/' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void](New-Item -ItemType Directory -Path $taskDir)
$taskReport = Join-Path $taskRoot 'target/qa/pin-enlarged-export/report.json'
if (Test-Path -LiteralPath $taskReport) { Copy-Item -LiteralPath $taskReport -Destination (Join-Path $taskDir 'previous-report.json') }
$taskVariables = @('RUSTSHOT_PIN_SMOKE_CASE','RUSTSHOT_QA_NO_RESIDENT_NOTIFY')
$taskOldValues = @{}
foreach ($taskVariable in $taskVariables) { $taskOldValues[$taskVariable] = [Environment]::GetEnvironmentVariable($taskVariable,'Process') }
try {
    $env:RUSTSHOT_PIN_SMOKE_CASE = 'enlarged_export'
    $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY = '1'
    $taskStarted = Get-Date
    $taskProcess = Start-Process -FilePath $taskExe -ArgumentList '--pin-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $taskDir 'stdout.log') -RedirectStandardError (Join-Path $taskDir 'stderr.log')
    $taskDeadline = (Get-Date).AddSeconds(90)
    while (-not $taskProcess.WaitForExit(1000)) {
        if ((Get-Date) -ge $taskDeadline) { throw "自有贴图放大导出验收超时：$($taskProcess.Id)，记录 $taskDir" }
    }
    $taskProcess.Refresh()
    if (Test-Path -LiteralPath $taskReport) { Copy-Item -LiteralPath $taskReport -Destination (Join-Path $taskDir 'report.json') }
    if ($taskProcess.ExitCode -ne 0) {
        Get-Content -LiteralPath (Join-Path $taskDir 'stderr.log')
        if (Test-Path -LiteralPath $taskReport) { Get-Content -LiteralPath $taskReport }
        throw "贴图放大导出原生验收失败：$taskDir"
    }
    if ((Get-Item -LiteralPath $taskReport).LastWriteTime -lt $taskStarted) { throw '未生成本次贴图放大导出报告' }
    $taskSummary = Get-Content -LiteralPath $taskReport -Raw | ConvertFrom-Json
    if ($taskSummary.ok -ne $true) { throw "贴图放大导出验收未通过：$taskDir" }
    [pscustomobject]@{path=$taskExe;sha256=(Get-FileHash -LiteralPath $taskExe).Hash;configuration=$Configuration;worker_pid=$taskProcess.Id;worker_exited=$taskProcess.HasExited;report=(Join-Path $taskDir 'report.json');ok=$true} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $taskDir 'binary.json') -Encoding utf8
    [pscustomobject]@{ok=$taskSummary.ok;groups=@($taskSummary.groups.group);source=$taskSummary.source;clipboard_written=$taskSummary.clipboard_written} | ConvertTo-Json -Depth 4 -Compress
    Write-Output $taskDir
} finally {
    foreach ($taskVariable in $taskVariables) { [Environment]::SetEnvironmentVariable($taskVariable,$taskOldValues[$taskVariable],'Process') }
}


