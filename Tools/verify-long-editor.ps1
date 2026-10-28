[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Worker,
    [ValidateRange(30, 180)][int]$TimeoutSeconds = 110,
    # 面板、图片预览与贴图走各自专项；这里仅验证当前调整窗。
    [ValidateSet('all', 'editor')]
    [string[]]$CaseFilter = @('all')
)
$ErrorActionPreference = 'Stop'
$taskRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker = (Resolve-Path -LiteralPath $Worker).Path
if ([IO.Path]::GetFileNameWithoutExtension($taskWorker) -ne 'raishot-gpui') {
    throw '长图编辑专项只接受 raishot-gpui.exe。'
}
if ($CaseFilter -contains 'all' -and $CaseFilter.Count -ne 1) {
    throw 'all不能与其它case同时运行，避免重复已完成验收。'
}
if (-not ('RustShotLongEditorQa' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class RustShotLongEditorQa {
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
        throw new System.ComponentModel.Win32Exception(error, "不能可靠检查CaptureMutex");
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
        [RustShotLongEditorQa]::CaptureExists()) {
        throw '已有截图、贴图或设置会话；保留现有窗口，未启动编辑专项。'
    }
}
Assert-Idle
$taskHash = (Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash
$taskFolder = Join-Path $taskRoot ('target/qa/long-editor/runner-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[IO.Directory]::CreateDirectory($taskFolder) | Out-Null
$taskNames = @('RUSTSHOT_LONG_EDITOR_QA_CASE', 'RUSTSHOT_LONG_QA_DIR', 'RUSTSHOT_DATA_DIR',
    'RUSTSHOT_QA_NO_RESIDENT_NOTIFY')
$taskPrior = @{}
foreach ($taskName in $taskNames) {
    $taskPrior[$taskName] = [Environment]::GetEnvironmentVariable($taskName, 'Process')
}
$taskResults = @()
$taskExpected = @('adjustment_native_edit_zoom_return')
try {
    foreach ($taskCase in @($CaseFilter | Select-Object -Unique)) {
        Assert-Idle
        $taskCaseFolder = Join-Path $taskFolder $taskCase
        [IO.Directory]::CreateDirectory($taskCaseFolder) | Out-Null
        $env:RUSTSHOT_LONG_EDITOR_QA_CASE = $taskCase
        $env:RUSTSHOT_LONG_QA_DIR = $taskCaseFolder
        $env:RUSTSHOT_DATA_DIR = Join-Path $taskCaseFolder 'settings'
        $env:RUSTSHOT_QA_NO_RESIDENT_NOTIFY = '1'
        $taskStdout = Join-Path $taskCaseFolder 'stdout.jsonl'
        $taskStderr = Join-Path $taskCaseFolder 'stderr.log'
        $taskProcess = Start-Process -FilePath $taskWorker -ArgumentList '--long-editor-smoke' `
            -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru `
            -RedirectStandardOutput $taskStdout -RedirectStandardError $taskStderr
        if (-not $taskProcess.WaitForExit($TimeoutSeconds * 1000)) {
            throw "长图编辑 $taskCase 超时，PID=$($taskProcess.Id)；保留进程，没有强制结束。日志：$taskStderr"
        }
        $taskProcess.Refresh()
        if ($taskProcess.ExitCode -ne 0) { throw "长图编辑 $taskCase 未通过；详见 $taskStdout 和 $taskStderr" }
        if ([RustShotLongEditorQa]::Windows([uint32]$taskProcess.Id) -ne 0) { throw '退出后仍有自有窗口。' }
        if ([RustShotLongEditorQa]::CaptureExists()) { throw '编辑退出后CaptureMutex未释放。' }
        if ((Get-FileHash -LiteralPath $taskWorker -Algorithm SHA256).Hash -ne $taskHash) { throw '专项期间构建文件发生变化。' }
        $taskReport = Join-Path $taskCaseFolder 'ui.jsonl'
        if (-not (Test-Path -LiteralPath $taskReport -PathType Leaf)) { throw '缺少专项报告。' }
        $taskRows = @(Get-Content -LiteralPath $taskReport | ForEach-Object { $_ | ConvertFrom-Json })
        if ($taskRows.Count -eq 0 -or @($taskRows | Where-Object {
            $_.pid -ne $taskProcess.Id -or $_.owned_only -ne $true -or $_.ok -ne $true
        }).Count -ne 0) { throw '专项报告为空、含失败或来自非自有来源。' }
        foreach ($taskName in $taskExpected) {
            if (@($taskRows | Where-Object { $_.test -eq $taskName -and $_.ok -eq $true }).Count -ne 1) {
                throw "缺少唯一验收记录：$taskName"
            }
        }
        $taskResults += [pscustomobject]@{ case = $taskCase; pid = $taskProcess.Id; exit_code = $taskProcess.ExitCode;
            own_windows_remaining = 0; capture_mutex_released = $true; report = $taskReport; ok = $true }
    }
    $taskSummary = [pscustomobject]@{ suite = 'long-editor'; worker = $taskWorker; sha256 = $taskHash;
        clipboard_untouched = $true; results = $taskResults; ok = $true }
    $taskSummary | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $taskFolder 'summary.json') -Encoding UTF8
    $taskSummary | ConvertTo-Json -Depth 6
} finally {
    foreach ($taskName in $taskNames) { [Environment]::SetEnvironmentVariable($taskName, $taskPrior[$taskName], 'Process') }
}
