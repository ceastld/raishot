[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Worker,
    [ValidateRange(20, 90)][int]$TimeoutSeconds = 45,
    [ValidateSet('native', 'complete', 'active', 'early_cancel', 'queued_keys', 'modifiers')]
    [string[]]$CaseFilter = @('native', 'complete', 'active', 'early_cancel', 'queued_keys', 'modifiers')
)
$ErrorActionPreference = 'Stop'
$taskRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker = (Resolve-Path -LiteralPath $Worker).Path
if ([IO.Path]::GetFileNameWithoutExtension($taskWorker) -ne 'raishot-gpui') {
    throw '首屏专项只接受 raishot-gpui.exe 的绝对路径。'
}
if (-not ('RustShotStartupSurfaceQa' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
public static class RustShotStartupSurfaceQa {
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern IntPtr OpenMutex(uint access, bool inherit, string name);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc callback, IntPtr context);
    delegate bool EnumProc(IntPtr window, IntPtr context);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
    public static bool CaptureExists() {
        IntPtr mutex = OpenMutex(0x00100000, false, @"Local\RustShot.MVP.Capture");
        if (mutex != IntPtr.Zero) { CloseHandle(mutex); return true; }
        int error = Marshal.GetLastWin32Error();
        if (error == 2) return false;
        throw new System.ComponentModel.Win32Exception(error, "无法可靠检查截图互斥体");
    }
    public static int Windows(uint pid) {
        int count = 0;
        EnumWindows((window, context) => {
            uint owner; GetWindowThreadProcessId(window, out owner);
            if (owner == pid) ++count;
            return true;
        }, IntPtr.Zero);
        return count;
    }
}
'@
}
function Assert-Idle {
    if (@(Get-Process -Name 'raishot-gpui' -ErrorAction SilentlyContinue).Count -ne 0 -or
        [RustShotStartupSurfaceQa]::CaptureExists()) {
        throw '已有截图、贴图或设置会话；保留现有窗口，未继续启动专项。'
    }
}
Assert-Idle
$taskHash = (Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash
$taskFolder = Join-Path $taskRoot ('target/qa/startup-surface/runner-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[IO.Directory]::CreateDirectory($taskFolder) | Out-Null
$taskNames = @('RUSTSHOT_SURFACE_QA_CASE', 'RUSTSHOT_SURFACE_QA_DIR', 'RUSTSHOT_DATA_DIR',
    'RUSTSHOT_QA_NO_RESIDENT_NOTIFY', 'RUSTSHOT_STARTUP_TRACE', 'RUSTSHOT_STARTUP_SURFACE',
    'RUSTSHOT_STARTUP_SERIAL')
$taskPrior = @{}
foreach ($taskName in $taskNames) {
    $taskPrior[$taskName] = [Environment]::GetEnvironmentVariable($taskName, 'Process')
}
$taskResults = @()
try {
    foreach ($taskCase in @($CaseFilter | Select-Object -Unique)) {
        Assert-Idle
        $taskCaseFolder = Join-Path $taskFolder $taskCase
        [IO.Directory]::CreateDirectory($taskCaseFolder) | Out-Null
        $env:RUSTSHOT_SURFACE_QA_CASE = $taskCase
        $env:RUSTSHOT_SURFACE_QA_DIR = $taskCaseFolder
        $env:RUSTSHOT_DATA_DIR = Join-Path $taskCaseFolder 'settings'
        $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY = '1'
        $env:RUSTSHOT_STARTUP_TRACE = '1'
        $env:RUSTSHOT_STARTUP_SURFACE = '1'
        [Environment]::SetEnvironmentVariable('RUSTSHOT_STARTUP_SERIAL', $null, 'Process')
        $taskStdout = Join-Path $taskCaseFolder 'stdout.jsonl'
        $taskStderr = Join-Path $taskCaseFolder 'stderr.log'
        $taskProcess = Start-Process -FilePath $taskWorker -ArgumentList '--startup-surface-smoke' `
            -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput $taskStdout -RedirectStandardError $taskStderr
        if (-not $taskProcess.WaitForExit($TimeoutSeconds * 1000)) {
            throw "首屏 $taskCase 专项超时，PID=$($taskProcess.Id)；保留自有窗口供检查，没有强制结束进程。日志：$taskStderr"
        }
        $taskProcess.Refresh()
        if ($taskProcess.ExitCode -ne 0) { throw "首屏 $taskCase 未通过；详见 $taskStdout 和 $taskStderr" }
        if ([RustShotStartupSurfaceQa]::Windows([uint32]$taskProcess.Id) -ne 0) {
            throw "首屏 $taskCase 正常退出后仍有自有原生窗口。"
        }
        if ([RustShotStartupSurfaceQa]::CaptureExists()) { throw '首屏专项退出后 CaptureMutex 未释放。' }
        if ((Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash -ne $taskHash) {
            throw '专项期间构建文件发生变化。'
        }
        $taskTrace = @(Get-Content -LiteralPath $taskStdout | Where-Object { $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
        $taskDrops = @($taskTrace | Where-Object { $_.phase -eq 'startup_surface_resources_drop' })
        if ($taskDrops.Count -eq 0 -or @($taskDrops | Where-Object { $_.pid -ne $taskProcess.Id -or $_.ok -ne $true }).Count -ne 0) {
            throw '首屏自有 bitmap/brush/DC 删除或原生线程 GDI 提交失败。'
        }
        $taskReport = Join-Path $taskCaseFolder 'ui.jsonl'
        if (-not (Test-Path -LiteralPath $taskReport -PathType Leaf)) { throw "缺少 $taskCase 的自有像素/交接报告。" }
        $taskRows = @(Get-Content -LiteralPath $taskReport | ForEach-Object { $_ | ConvertFrom-Json })
        if (@($taskRows | Where-Object { $_.ok -eq $false }).Count -ne 0) { throw "$taskCase 报告包含失败。" }
        $taskExpected = if ($taskCase -eq 'native') {
            @('native_surface_exact_pixels', 'native_surface_reverse_drag',
              'native_surface_reverse_recycled', 'native_surface_escape_recycled',
              'native_surface_escape_cancel_gesture', 'native_surface_right_cancel_gesture',
              'native_surface_right_recycled', 'native_surface_mutex_recycled')
        } elseif ($taskCase -eq 'early_cancel') {
            @('surface_owned_fixture_frozen', 'surface_early_cancel_before_gpui',
              'surface_early_cancel_recycled')
        } else {
            @('surface_owned_fixture_frozen', 'surface_pre_gpui_gesture',
              'surface_gpui_handoff', 'surface_gpui_input_cancel_recycled')
        }
        if ($taskCase -eq 'active') { $taskExpected += 'surface_active_survives_gpui_present' }
        if ($taskCase -eq 'queued_keys') { $taskExpected += 'surface_queued_keys_replayed' }
        if ($taskCase -eq 'modifiers') { $taskExpected += @('surface_modifier_snapshot', 'surface_modifier_save_cancel') }
        foreach ($taskName in $taskExpected) {
            $taskMatches = @($taskRows | Where-Object { $_.test -eq $taskName -and $_.pid -eq $taskProcess.Id })
            if ($taskMatches.Count -ne 1 -or -not $taskMatches[0].ok -or -not $taskMatches[0].owned_only) {
                throw "缺少本进程通过记录：$taskCase/$taskName"
            }
        }
        $taskFinal = @(Get-Content -LiteralPath $taskStdout | Where-Object { $_.StartsWith('{') } |
            ForEach-Object { $_ | ConvertFrom-Json } |
            Where-Object { $_.phase -eq 'startup_surface_smoke_complete' -and $_.stage -eq $taskCase -and $_.ok })
        if ($taskFinal.Count -ne 1) { throw "$taskCase 未完成真实取消/回收最终检查。" }
        $taskResults += [pscustomobject]@{case=$taskCase;pid=$taskProcess.Id;exit_code=0;cases=$taskExpected.Count;
            report=$taskReport;stdout=$taskStdout;stderr=$taskStderr;capture_mutex_released=$true;owned_only=$true}
    }
} finally {
    foreach ($taskName in $taskNames) {
        [Environment]::SetEnvironmentVariable($taskName, $taskPrior[$taskName], 'Process')
    }
}
$taskSummary = [pscustomobject]@{ok=$true;worker=$taskWorker;sha256=$taskHash;owned_only=$true;results=$taskResults}
$taskSummary | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $taskFolder 'summary.json') -Encoding utf8
$taskSummary | ConvertTo-Json -Depth 6 -Compress
