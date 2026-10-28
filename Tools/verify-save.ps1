[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Worker,
    [ValidateRange(20, 120)][int]$TimeoutSeconds = 90
)
$ErrorActionPreference = 'Stop'
$taskRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker = (Resolve-Path -LiteralPath $Worker).Path
$taskHash = (Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash
$taskFolder = Join-Path $taskRoot ('target/qa/save/runner-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[IO.Directory]::CreateDirectory($taskFolder) | Out-Null
$taskStdout = Join-Path $taskFolder 'stdout.jsonl'
$taskStderr = Join-Path $taskFolder 'stderr.log'
# 只启动新的 QA 工作进程；既有托盘、截图和贴图窗口均不参与专项。
$taskProcess = Start-Process -FilePath $taskWorker -ArgumentList '--save-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $taskStdout -RedirectStandardError $taskStderr
if (-not $taskProcess.WaitForExit($TimeoutSeconds * 1000)) {
    throw "保存专项没有按时结束，PID=$($taskProcess.Id)；保留自有窗口供检查，未结束任何进程。"
}
$taskProcess.Refresh()
if ($taskProcess.ExitCode -ne 0) { throw "保存专项失败，详见 $taskStdout 和 $taskStderr" }
if ((Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash -ne $taskHash) { throw '专项期间构建文件发生变化，请用最终产物复核。' }
$taskRows = @(Get-Content -LiteralPath $taskStdout | Where-Object { $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
$taskComplete = @($taskRows | Where-Object { $_.phase -eq 'save_smoke_complete' })
if ($taskComplete.Count -ne 1 -or -not $taskComplete[0].ok -or -not $taskComplete[0].synthetic_only -or $taskComplete[0].pid -ne $taskProcess.Id) { throw '缺少合成图专项完成证据。' }
$taskExpected = @(
    'save_encoding_png_alpha_bmp_white',
    'save_encoding_jpeg_real_quality',
    'save_quick_no_overwrite_failure_no_history',
    'save_panel_template_manual_cancel_no_writes',
    'save_panel_png_bmp_success_remember_reload',
    'save_panel_recent_directory_independent_quick_default',
    'save_panel_export_failure_keeps_session',
    'save_panel_same_name_confirm_keep_both',
    'save_panel_protected_original_never_overwritten',
    'save_owner_image_cancel_resumes_editing',
    'save_owner_pin_cancel_resumes_canvas',
    'save_owner_capture_quick_save_closes_records_once',
    'save_windows_recycled'
)
foreach ($taskCase in $taskExpected) {
    $taskMatches = @($taskRows | Where-Object { $_.suite -eq 'save' -and $_.test -eq $taskCase })
    if ($taskMatches.Count -ne 1 -or -not $taskMatches[0].ok -or $taskMatches[0].pid -ne $taskProcess.Id) { throw "保存专项缺少通过记录：$taskCase" }
}
if (@($taskRows | Where-Object { $_.ok -eq $false }).Count -ne 0) { throw '保存专项报告包含失败。' }
$taskReport = [IO.Path]::GetFullPath($taskComplete[0].report)
$taskQaRoot = [IO.Path]::GetFullPath((Join-Path $taskRoot 'target/qa/save')) + [IO.Path]::DirectorySeparatorChar
if (-not $taskReport.StartsWith($taskQaRoot, [StringComparison]::OrdinalIgnoreCase) -or -not (Test-Path -LiteralPath $taskReport -PathType Leaf)) { throw '专项报告路径不在隔离 QA 目录中。' }
[pscustomobject]@{ok=$true;worker=$taskWorker;sha256=$taskHash;pid=$taskProcess.Id;cases=$taskExpected.Count;report=$taskReport;stdout=$taskStdout;stderr=$taskStderr} | ConvertTo-Json -Compress
