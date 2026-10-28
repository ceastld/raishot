[CmdletBinding()]
param(
    [Parameter(Mandatory=$true)][string]$Worker,
    [ValidateSet('capture','pin')][string[]]$CaseFilter=@('capture','pin'),
    [ValidateSet('all','remaining')][string]$CaptureStage='all'
)
$ErrorActionPreference='Stop'
$taskRoot=(Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$taskWorker=(Resolve-Path -LiteralPath $Worker).Path
$taskHash=(Get-FileHash -LiteralPath $taskWorker).Hash
if (-not ('RustShotRadiusHandleQa' -as [type])) {
    Add-Type @'
using System;using System.Runtime.InteropServices;
public static class RustShotRadiusHandleQa {
 delegate bool Callback(IntPtr h,IntPtr d);
 [DllImport("user32.dll")]static extern bool EnumWindows(Callback callback,IntPtr data);
 [DllImport("user32.dll")]static extern uint GetWindowThreadProcessId(IntPtr h,out uint pid);
 [DllImport("kernel32.dll",CharSet=CharSet.Unicode,SetLastError=true)]static extern IntPtr OpenMutex(uint a,bool inherit,string n);
 [DllImport("kernel32.dll")]static extern bool CloseHandle(IntPtr h);
 public static int Windows(uint pid){int count=0;EnumWindows((h,d)=>{uint p;GetWindowThreadProcessId(h,out p);if(p==pid)count++;return true;},IntPtr.Zero);return count;}
 public static bool CaptureExists(){IntPtr h=OpenMutex(0x00100000,false,@"Local\RustShot.MVP.Capture");if(h!=IntPtr.Zero){CloseHandle(h);return true;}int error=Marshal.GetLastWin32Error();if(error==2)return false;throw new System.ComponentModel.Win32Exception(error);}
}
'@
}
if ([RustShotRadiusHandleQa]::CaptureExists()) {throw '已有截图会话，保留用户操作，停止专项。'}
$taskEnvKeys=@('RUSTSHOT_PIN_SMOKE_CASE','RUSTSHOT_QA_NO_RESIDENT_NOTIFY','RUSTSHOT_RADIUS_SKIP_VERIFIED_RECT')
$taskOldEnv=@{}
foreach($taskKey in $taskEnvKeys){$taskOldEnv[$taskKey]=[Environment]::GetEnvironmentVariable($taskKey,'Process')}
$taskOutput=Join-Path $taskRoot ('target/qa/radius-handle-native/'+(Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
[void][IO.Directory]::CreateDirectory($taskOutput)
try {
    [Environment]::SetEnvironmentVariable('RUSTSHOT_PIN_SMOKE_CASE','radius','Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_QA_NO_RESIDENT_NOTIFY','1','Process')
    [Environment]::SetEnvironmentVariable('RUSTSHOT_RADIUS_SKIP_VERIFIED_RECT',$(if($CaptureStage -eq 'remaining'){'1'}else{'0'}),'Process')
    foreach($taskCase in $CaseFilter) {
        if ([RustShotRadiusHandleQa]::CaptureExists()) {throw '有新截图会话，停止后续专项。'}
        $taskFolder=Join-Path $taskOutput $taskCase
        [void][IO.Directory]::CreateDirectory($taskFolder)
        $taskStdout=Join-Path $taskFolder 'stdout.jsonl'
        $taskStderr=Join-Path $taskFolder 'stderr.log'
        $taskArgument=if($taskCase -eq 'pin'){'--pin-smoke'}else{'--radius-handle-smoke'}
        $taskProcess=Start-Process -FilePath $taskWorker -ArgumentList $taskArgument -WorkingDirectory $taskRoot -WindowStyle Hidden -PassThru -RedirectStandardOutput $taskStdout -RedirectStandardError $taskStderr
        if(-not $taskProcess.WaitForExit(45000)){throw "圆角把手 $taskCase 专项超时；保留PID=$($taskProcess.Id)，未强制结束。日志：$taskFolder"}
        $taskProcess.Refresh()
        if($taskProcess.ExitCode -ne 0){throw "圆角把手 $taskCase 专项失败：$taskFolder"}
        $taskExpected=if($taskCase -eq 'capture'){@('native_radius_handle_noop_preview_escape_cancel_preserves_redo','native_radius_handle_rectangle_four_corners_rotated_undo_redo','native_radius_handle_vector_chrome_visible_excluded_from_export','native_radius_handle_four_corners_zoom_pan_stable_geometry','native_radius_handle_spotlight_local_radius_shared_layer_preserved')}else{@('native_radius_handle_pin_four_corners_shared_editor_toolbar_stable')}
        if($taskCase -eq 'capture' -and $CaptureStage -eq 'remaining'){$taskExpected=@($taskExpected|Select-Object -Skip 2)}
        $taskRows=@(Get-Content -LiteralPath $taskStdout|Where-Object{$_.StartsWith('{')}|ForEach-Object{$_|ConvertFrom-Json})
        foreach($taskName in $taskExpected){if(@($taskRows|Where-Object{$_.test -eq $taskName -and $_.ok -eq $true}).Count -ne 1){throw "圆角把手报告缺少 $taskName"}}
        if(@($taskRows|Where-Object{$_.ok -eq $false}).Count -ne 0){throw '报告包含失败'}
        if([RustShotRadiusHandleQa]::Windows([uint32]$taskProcess.Id) -ne 0 -or [RustShotRadiusHandleQa]::CaptureExists()){throw '专项退出后自有窗口/互斥体未回收'}
        if((Get-FileHash -LiteralPath $taskWorker).Hash -ne $taskHash){throw '专项期间产物变化'}
        $taskSummary=[pscustomobject]@{ok=$true;case=$taskCase;pid=$taskProcess.Id;exit_code=$taskProcess.ExitCode;worker=$taskWorker;sha256=$taskHash;tests=$taskExpected;log=$taskStdout;windows_recycled=$true;capture_mutex_released=$true}
        $taskSummary|ConvertTo-Json -Depth 4|Set-Content -LiteralPath (Join-Path $taskFolder 'summary.json') -Encoding utf8
        $taskSummary|ConvertTo-Json -Depth 4 -Compress
    }
} finally {foreach($taskKey in $taskEnvKeys){[Environment]::SetEnvironmentVariable($taskKey,$taskOldEnv[$taskKey],'Process')}}
