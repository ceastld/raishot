[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Worker,
    [ValidateRange(20, 180)][int]$TimeoutSeconds = 90
)
$ErrorActionPreference = 'Stop'
function Normalize-QAPath([string]$path) {
    if ($path.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {
        $path = '\\' + $path.Substring(8)
    } elseif ($path.StartsWith('\\?\', [StringComparison]::OrdinalIgnoreCase)) {
        $path = $path.Substring(4)
    }
    [IO.Path]::GetFullPath($path)
}
$taskRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker = (Resolve-Path -LiteralPath $Worker).Path
$taskHash = (Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash
$taskFolder = Join-Path $taskRoot ('target/qa/pin-workspace/runner-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[IO.Directory]::CreateDirectory($taskFolder) | Out-Null
$taskEnvironmentNames = @('RUSTSHOT_PIN_WORKSPACE_QA_DIR', 'RUSTSHOT_PIN_WORKSPACE_QA_PHASE', 'RUSTSHOT_DATA_DIR', 'RUSTSHOT_QA_NO_RESIDENT_NOTIFY')
$taskPreviousEnvironment = @{}
foreach ($taskName in $taskEnvironmentNames) { $taskPreviousEnvironment[$taskName] = [Environment]::GetEnvironmentVariable($taskName, 'Process') }
$taskReports = @()
$taskProcesses = @()
try {
    [Environment]::SetEnvironmentVariable('RUSTSHOT_PIN_WORKSPACE_QA_DIR', $taskFolder, 'Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_DATA_DIR', (Join-Path $taskFolder 'settings'), 'Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_QA_NO_RESIDENT_NOTIFY', '1', 'Process')
    foreach ($taskPhase in @('seed', 'resume')) {
        [Environment]::SetEnvironmentVariable('RUSTSHOT_PIN_WORKSPACE_QA_PHASE', $taskPhase, 'Process')
        $taskStdout = Join-Path $taskFolder ($taskPhase + '-stdout.jsonl')
        $taskStderr = Join-Path $taskFolder ($taskPhase + '-stderr.log')
        # 两阶段为全新的自有工作进程；不会附着或结束现有用户窗口。
        $taskProcess = Start-Process -FilePath $taskWorker -ArgumentList '--pin-workspace-smoke' -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $taskStdout -RedirectStandardError $taskStderr
        $taskProcesses += $taskProcess.Id
        if (-not $taskProcess.WaitForExit($TimeoutSeconds * 1000)) { throw "贴图工作区 $taskPhase 阶段超时，PID=$($taskProcess.Id)；没有结束任何进程，详见 $taskStdout / $taskStderr。" }
        $taskProcess.Refresh()
        if ($taskProcess.ExitCode -ne 0) { throw "贴图工作区 $taskPhase 阶段失败，详见 $taskStdout / $taskStderr。" }
        $taskRows = @(Get-Content -LiteralPath $taskStdout | Where-Object { $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
        $taskComplete = @($taskRows | Where-Object { $_.phase -eq 'pin_workspace_smoke_complete' -and $_.stage -eq $taskPhase })
        if ($taskComplete.Count -ne 1 -or -not $taskComplete[0].ok -or -not $taskComplete[0].synthetic_only -or $taskComplete[0].pid -ne $taskProcess.Id) { throw "缺少 $taskPhase 阶段的真实自有窗口完成证据。" }
        if (@($taskRows | Where-Object { $_.ok -eq $false }).Count -ne 0) { throw "$taskPhase 阶段报告含失败。" }
        $taskExpected = if ($taskPhase -eq 'seed') {
            @('workspace_groups_ui_order_title_move', 'workspace_manual_hidden_group_projection', 'workspace_native_close_current_group_restore_retry', 'workspace_processing_close_rotated_canonical', 'workspace_disabled_restore_forgets_asset', 'workspace_exit_preserves_active', 'workspace_seed_windows_recycled')
        } else {
            @('workspace_new_process_restores_groups_views', 'workspace_fault_retry_visible_placement', 'workspace_resume_windows_recycled')
        }
        foreach ($taskCase in $taskExpected) {
            $taskMatches = @($taskRows | Where-Object { $_.suite -eq 'pin-workspace' -and $_.test -eq $taskCase -and $_.stage -eq $taskPhase })
            if ($taskMatches.Count -ne 1 -or -not $taskMatches[0].ok -or $taskMatches[0].pid -ne $taskProcess.Id) { throw "贴图工作区专项缺少通过记录：$taskCase" }
        }
        $taskReport = Normalize-QAPath $taskComplete[0].report
        $taskQaRoot = (Normalize-QAPath $taskFolder) + [IO.Path]::DirectorySeparatorChar
        if (-not $taskReport.StartsWith($taskQaRoot, [StringComparison]::OrdinalIgnoreCase) -or -not (Test-Path -LiteralPath $taskReport -PathType Leaf)) { throw '专项报告不在本次隔离目录中。' }
        $taskReports += $taskReport
        if ((Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash -ne $taskHash) { throw '验收过程中构建文件变化，请用同一个最终产物重试。' }
    }
} finally {
    foreach ($taskName in $taskEnvironmentNames) { [Environment]::SetEnvironmentVariable($taskName, $taskPreviousEnvironment[$taskName], 'Process') }
}
if ($taskProcesses.Count -ne 2 -or $taskProcesses[0] -eq $taskProcesses[1]) { throw '跨会话恢复没有经过两个独立进程。' }
[pscustomobject]@{ok=$true;worker=$taskWorker;sha256=$taskHash;pids=$taskProcesses;cases=10;reports=$taskReports;folder=$taskFolder;synthetic_only=$true} | ConvertTo-Json -Compress
