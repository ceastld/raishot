[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Worker,
    [ValidateRange(20, 90)][int]$TimeoutSeconds = 75
)
$ErrorActionPreference = 'Stop'
$taskRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker = (Resolve-Path -LiteralPath $Worker).Path
if (@(Get-Process -Name 'raishot-gpui' -ErrorAction SilentlyContinue).Count -ne 0) {
    throw '已有截图、贴图或设置会话；保留现有窗口，未启动专项。'
}
$taskHash = (Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash
$taskFolder = Join-Path $taskRoot ('target/qa/customization/runner-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[IO.Directory]::CreateDirectory($taskFolder) | Out-Null
$taskStdout = Join-Path $taskFolder 'stdout.jsonl'
$taskStderr = Join-Path $taskFolder 'stderr.log'
$taskProcess = Start-Process -FilePath $taskWorker -ArgumentList '--customization-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $taskStdout -RedirectStandardError $taskStderr
if (-not $taskProcess.WaitForExit($TimeoutSeconds * 1000)) {
    throw "设置专项超时，PID=$($taskProcess.Id)；保留自有窗口供检查。"
}
$taskProcess.Refresh()
if ($taskProcess.ExitCode -ne 0) { throw "设置专项未通过；详见 $taskStdout 和 $taskStderr" }
if ((Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash -ne $taskHash) { throw '专项期间构建文件发生变化。' }
$taskRows = @(Get-Content -LiteralPath $taskStdout | Where-Object { $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
$taskExpected = @(
    'native_settings_record_printscreen_and_persist',
    'native_settings_cancel_disable_preset_and_reopen',
    'native_settings_toolbar_reset_hide_reorder_and_persist',
    'native_capture_shared_order_fixed_actions_and_hidden_tool_access',
    'native_pin_shared_order_fixed_actions_and_hidden_tool_access',
    'native_customization_owned_windows_cleaned'
)
foreach ($taskCase in $taskExpected) {
    $taskMatches = @($taskRows | Where-Object { $_.test -eq $taskCase })
    if ($taskMatches.Count -ne 1 -or -not $taskMatches[0].ok) { throw "缺少通过记录：$taskCase" }
}
if (@($taskRows | Where-Object { $_.ok -eq $false }).Count -ne 0) { throw '报告包含失败。' }
$taskNativeReport = Join-Path $taskRoot "target/qa/customization/ui-$($taskProcess.Id).jsonl"
if (-not (Test-Path -LiteralPath $taskNativeReport -PathType Leaf)) { throw '缺少本进程原生报告。' }
[pscustomobject]@{ok=$true;worker=$taskWorker;sha256=$taskHash;pid=$taskProcess.Id;cases=$taskExpected.Count;report=$taskNativeReport;stdout=$taskStdout;stderr=$taskStderr} | ConvertTo-Json -Compress
